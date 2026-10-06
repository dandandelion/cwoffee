import { pageLog } from './pagelog'

// Resolved once: the plugin module does not exist in a plain browser build, and a
// static import would make the preview crash at load rather than at click.
let opener: ((url: string) => Promise<void>) | null | undefined

// Exported so the self-test can assert the opener is present AND that its scope
// refuses non-http schemes. Probing with file:// proves both without launching a
// browser on the user's machine.
export async function linkProbe(href: string): Promise<string> {
  if (!href) return 'empty'
  if (opener === undefined) {
    try {
      const mod = await import('@tauri-apps/plugin-opener')
      opener = (u: string) => mod.openUrl(u)
      pageLog('opener plugin available')
    } catch {
      opener = null
      pageLog('opener plugin unavailable: links stay inert')
    }
  }
  if (!opener) return 'inert: no opener'
  try {
    await opener(href)
    return 'opened'
  } catch (err) {
    // A refused scope or a missing desktop handler must be visible, not silent.
    return `failed: ${err}`
  }
}

async function openExternal(href: string) {
  const st = await linkProbe(href)
  pageLog(`link ${st}: ${href}`)
}
/**
 * The transcript is rendered imperatively, not by the framework.
 *
 * Reason: a 50 tok/s stream means 50 updates a second. Re-rendering a message
 * list per token is the single easiest way to make a local-model UI feel worse
 * than the model actually is (PLAN.md §9 rule 3). So deltas become text nodes
 * appended to a node we already hold, batched into one flush per animation
 * frame, and the framework only re-renders on structural changes.
 *
 * Markdown is deliberately NOT parsed here. It is parsed once, at stream end,
 * by `renderMarkdown` — which for now is a plain-text pass. Half-parsing
 * markdown per token is how you get a UI that visibly reflows mid-sentence.
 */

import type { AttachmentMeta, EngineEvent, ExecStream, FileChange, StoredMessage } from './events'
import { renderMarkdown, LiveMarkdown, revealOn, revealWords, revealDuration, REVEAL_ANIM_MS } from './markdown'
import { fmtThink } from './think'
import { openPreview, inlineSvg } from './preview'

/** Clipboard feedback without a toast system: the button says it, briefly. */
function markCopied(btn: HTMLElement) {
  const prev = btn.textContent ?? 'copy'
  btn.textContent = 'copied'
  btn.classList.add('done')
  window.setTimeout(() => {
    btn.textContent = prev
    btn.classList.remove('done')
  }, 1200)
}
import type { OutputFile, QuestionRequested } from './events'

function fmtSize(n: number): string {
  if (n < 1024) return `${n} B`
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} kB`
  return `${(n / (1024 * 1024)).toFixed(1)} MB`
}

function shortMime(mime: string): string {
  const sub = mime.split('/')[1] ?? mime
  return sub.split(';')[0]
}

/**
 * The mark on a file chip, as inline SVG.
 *
 * Emoji were the obvious choice and the wrong one: they render in full colour from
 * a system font, so a chat drawn in two warm greys suddenly carries a blue picture
 * frame and a yellow folder. These share the composer icons' 16px box, stroke
 * weight and round joins, and take their colour from `currentColor`.
 */
function fileIcon(mime: string): SVGElement {
  const NS = 'http://www.w3.org/2000/svg'
  const svg = document.createElementNS(NS, 'svg')
  svg.setAttribute('viewBox', '0 0 16 16')
  svg.setAttribute('width', '16')
  svg.setAttribute('height', '16')
  svg.setAttribute('class', 'file-glyph')
  svg.setAttribute('aria-hidden', 'true')
  const path = (d: string) => {
    const el = document.createElementNS(NS, 'path')
    el.setAttribute('d', d)
    svg.appendChild(el)
  }
  if (mime.startsWith('image/')) {
    path('M3.4 2.4h7.2l2.6 2.6v8.6H3.4z')
    path('M10.6 2.4v2.6h2.6')
    path('M4.8 11.4l2.3-2.5 1.9 2 1.6-1.7 1.4 1.6')
  } else if (mime.includes('spreadsheet') || mime.endsWith('csv')) {
    path('M3 2.6h10v10.8H3z')
    path('M3 6.2h10')
    path('M6.4 6.2v7.2')
    path('M9.8 6.2v7.2')
  } else {
    path('M3.4 2.4h7.2l2.6 2.6v8.6H3.4z')
    path('M10.6 2.4v2.6h2.6')
    path('M5.8 8.4h4.4')
    path('M5.8 10.8h3')
  }
  return svg
}

function svgIcon(paths: string[], size = 14): SVGElement {
  const NS = 'http://www.w3.org/2000/svg'
  const svg = document.createElementNS(NS, 'svg')
  svg.setAttribute('viewBox', '0 0 16 16')
  svg.setAttribute('width', String(size))
  svg.setAttribute('height', String(size))
  svg.setAttribute('class', 'tact-glyph')
  svg.setAttribute('aria-hidden', 'true')
  for (const d of paths) {
    const el = document.createElementNS(NS, 'path')
    el.setAttribute('d', d)
    svg.appendChild(el)
  }
  return svg
}

const iconCopy = () =>
  svgIcon(['M5.6 5.6h7v7.4h-7z', 'M10.4 3.4H3.4v7.4'])
const iconDoc = () =>
  svgIcon(['M3.6 2.6h6l2.8 2.8v8H3.6z', 'M9.6 2.6v2.8h2.8', 'M8 8.2v4', 'M6.2 10.4 8 12.2l1.8-1.8'])
const iconBranch = () =>
  svgIcon(['M5 3.6v7.2', 'M5 12.4a1.4 1.4 0 1 0 0 .1', 'M5 3.5a1.4 1.4 0 1 0 0 .1', 'M11.4 5.6a1.4 1.4 0 1 0 0 .1', 'M11.4 6.9c0 2.4-2.6 2.6-4.4 3.6'])

/**
 * Clipboard, with a way out.
 *
 * `navigator.clipboard` is the right API and is refused by WebKitGTK depending on
 * focus and permissions; a silent failure would leave the user pasting something
 * from an hour ago. The execCommand path is deprecated everywhere but still works
 * in the one webview this app ships, so try both and report which one failed.
 */
async function copyToClipboard(text: string): Promise<boolean> {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text)
      return true
    }
  } catch {
    /* fall through */
  }
  try {
    const ta = document.createElement('textarea')
    ta.value = text
    ta.style.position = 'fixed'
    ta.style.top = '-1000px'
    ta.style.opacity = '0'
    document.body.appendChild(ta)
    ta.focus()
    ta.select()
    const ok = document.execCommand('copy')
    ta.remove()
    return ok
  } catch {
    return false
  }
}

export type BlockKind = 'user' | 'reasoning' | 'answer' | 'tool' | 'note' | 'warn' | 'fail'


interface ToolRefs {
  card: HTMLElement
  /** The tool this card belongs to; the header subject is derived from it. */
  name: string
  /** Path / url / query the call is about, shown in the header. */
  subject: HTMLElement
  args: HTMLElement
  out: HTMLElement
  status: HTMLElement
  /** args + output together; this is what collapses. */
  detail: HTMLElement
  /** `+12 -3`: what this call did to the files, filled in when it finishes. */
  stat: HTMLElement
  stdout: string
  stderr: string
}

export class Transcript {
  private root: HTMLElement
  private turn: HTMLElement | null = null
  private textBuf: HTMLElement | null = null
  private textKind: BlockKind | null = null
  private tools = new Map<string, ToolRefs>()
  private pending: string[] = []
  private raf = 0
  /** Collected during a turn so it can be committed once, at stream end. */
  private collected: { answer: string; reasoning: string } = { answer: '', reasoning: '' }
  // Thinking blocks are clamped to a few lines by default. While the model is
  // thinking they follow the tail (the newest thought is the interesting one);
  // once the turn is over they snap back to the start, which is where a reader
  // scanning a finished block expects to begin.
  private reasoningEls: HTMLElement[] = []
  /**
   * Answer bodies created this turn. Markdown is applied to them exactly once,
   * at stream end — never per delta.
   */
  private answerEls: HTMLElement[] = []
  // Live markdown state for the answer block currently streaming.
  private liveEl: HTMLElement | null = null
  /** Files produced so far this turn, deduped by path, and the one row that
   *  shows them. */
  private turnFiles: OutputFile[] = []
  private questionCards = new Map<number, HTMLElement>()
  private fileStrip: HTMLElement | null = null
  private live = new LiveMarkdown()
  /**
   * How much of `live` has been turned into nodes. The streaming paint is
   * append-only: this only ever moves forward, and the only thing it decides is
   * which characters have not been put on screen yet.
   */
  private livePainted = 0
  /**
   * Words per second, smoothed. The reveal duration comes from this so the
   * animation is the length of the gap it is filling rather than a constant that
   * a fast model outruns.
   */
  private wordRate = 0
  private lastPaintAt = 0
  private lastLiveRender = 0
  private liveTimer = 0

  /** Set by the shell: opening a file is a Tauri concern, not a renderer one. */
  onOpenFile: ((path: string) => void) | null = null
  /**
   * Set by the shell. Export is a backend operation - the file has to land in the
   * workspace under the same containment rules as every other generated file - so
   * the transcript asks rather than doing it, and gets an OutputFile back to show.
   */
  onExport: ((session: number) => Promise<OutputFile>) | null = null
  /** Feedback for actions the user can otherwise not tell worked (copy, share). */
  onToast: ((msg: string, bad?: boolean) => void) | null = null
  /** Conversation the transcript is showing; export is addressed to it. */
  sessionId = 0
  /** Answer text of the live turn, kept so "copy" copies what the user read. */
  private turnAnswer: string[] = []

  constructor(root: HTMLElement) {
    this.root = root
    // A link that navigates replaces the whole app with a web page, which is
    // worse than a dead link, so default navigation is always prevented. The URL
    // then goes to Tauri's opener plugin, which checks it against the declared
    // scope. Outside Tauri (the vite preview) there is no opener, so the link
    // stays inert rather than hijacking the window.
    root.addEventListener('click', (e) => {
      const t = e.target as HTMLElement | null
      const act = t?.closest?.('.code-act') as HTMLElement | null
      if (act) {
        e.preventDefault()
        const blk = act.closest('.codeblk') as HTMLElement | null
        const code = blk?.querySelector('pre code') as HTMLElement | null
        if (!blk || !code) return
        const kind = (blk.dataset.lang ?? '') as 'html' | 'svg'
        const src = code.textContent ?? ''
        switch (act.dataset.act) {
          case 'copy':
            markCopied(act)
            void navigator.clipboard?.writeText(src).catch((err) => pageLog(`copy failed: ${err}`))
            break
          case 'preview':
            if (kind === 'html' || kind === 'svg') openPreview(kind, src)
            break
          case 'inline':
            inlineSvg(blk, src)
            break
        }
        return
      }
      const chip = t?.closest?.('.filechip') as HTMLElement | null
      if (chip) {
        e.preventDefault()
        const path = chip.dataset.path
        if (path && this.onOpenFile) this.onOpenFile(path)
        return
      }
      // A clamped thinking block expands in place. While it is collapsed the
      // whole block is the target - three lines of italic text is a small and
      // annoying thing to aim at. Once expanded, only the label collapses it,
      // so selecting text inside a long block does not hide it.
      const rblk = t?.closest?.('.blk-reasoning')
      if (rblk) {
        const body = rblk.querySelector('.blk-body') as HTMLElement | null
        const head = rblk.querySelector('.blk-head') as HTMLElement | null
        if (!body) return
        const open = body.classList.contains('expanded')
        if (open && !t?.closest?.('.blk-head')) return
        const nowOpen = body.classList.toggle('expanded')
        head?.classList.toggle('open', nowOpen)
        rblk.classList.toggle('is-open', nowOpen)
        if (nowOpen) body.scrollTop = 0
        return
      }
      const a = t?.closest?.('a')
      if (!a) return
      e.preventDefault()
      const href = a.getAttribute('href') ?? ''
      void openExternal(href)
    })
  }

  clear() {
    this.root.textContent = ''
    this.turn = null
    this.textBuf = null
    this.tools.clear()
    this.answerEls = []
    this.liveEl = null
    this.setLive(null)
    this.live = new LiveMarkdown()
    this.turnFiles = []
    this.fileStrip = null
    this.turnAnswer = []
    this.livePainted = 0
    this.wordRate = 0
    this.lastPaintAt = 0
  }

  /**
   * Mark the one block that is currently producing output. The indicator is on
   * the latest active block only - several pulsing at once reads as noise - and
   * it is removed outright at turn end rather than left idle, so a settled
   * transcript carries no animation at all.
   */
  private setLive(body: HTMLElement | null) {
    // A tool card is its own block, so its marker belongs on the card. Taking the
    // parent put `live` on the turn, which matched no CSS: a running command had no
    // indicator at all while a running answer did.
    const wrap = body?.classList?.contains('blk') ? body : (body?.parentElement ?? null)
    if (this.liveWrap === wrap) return
    this.liveWrap?.classList.remove('live')
    this.liveWrap = wrap
    wrap?.classList.add('live')
  }

  private liveWrap: HTMLElement | null = null

  /** Total answer text streamed so far this turn. Used by the render self-report. */
  get streamedChars() {
    return this.collected.answer.length
  }

  private el(cls: string, text?: string): HTMLElement {
    const d = document.createElement('div')
    d.className = cls
    if (text !== undefined) d.textContent = text
    return d
  }

  private ensureTurn() {
    if (this.turn) return
    this.turn = this.el('turn')
    this.root.appendChild(this.turn)
  }

  /** Start a new visual block. Structural change: allowed to touch layout. */
  private block(kind: BlockKind, label: string): HTMLElement {
    this.ensureTurn()
    const wrap = this.el(`blk blk-${kind}`)
    const head = this.el('blk-head', label)
    const body = this.el('blk-body')
    wrap.append(head, body)
    this.turn!.appendChild(wrap)
    this.scrollIfNeeded()
    return body
  }

  /**
   * Settle the thinking block that just ended.
   *
   * This used to happen once, at turn end, so every reasoning block of a long turn
   * read "thinking" - and stayed expanded - until the answer finished. A block
   * ends when the model stops reasoning: the first content delta or tool call
   * after it is the period at the end of that sentence, and that is when the
   * header becomes the useful fact (how long it took) and the body collapses.
   */
  private closeReasoning() {
    this.settleReasoning()
    if (this.textKind === 'reasoning' && this.textBuf) {
      this.textBuf.classList.remove('streaming')
    }
  }

  /** Collapse the thinking blocks of a finished turn back to their first lines. */
  private settleReasoning() {
    for (const el of this.reasoningEls) {
      el.classList.add('settled')
      el.scrollTop = 0
      // The label changes once thinking is over: while it runs "thinking" is
      // the truth, afterwards the useful fact is how long it took.
      const started = Number(el.dataset.thinkStart ?? 0)
      if (started) {
        const head = el.parentElement?.querySelector('.blk-head')
        if (head) head.textContent = fmtThink(performance.now() - started)
      }
    }
    this.reasoningEls = []
  }

  private flush() {
    this.raf = 0
    if (!this.pending.length) return
    const joined = this.pending.join('')
    this.pending = []
    if (!this.textBuf) return
    if (this.textKind === 'answer') {
      this.turnAnswer.push(joined)
      // Answers format as they arrive. The naive version re-parses the whole
      // answer on every frame, which is what makes streaming UIs stutter on a
      // local model; instead only the still-growing tail block is re-parsed and
      // finished blocks are rendered exactly once.
      this.live.push(joined)
      this.renderLive(false)
    } else if (this.textKind === 'reasoning') {
      // Reasoning is plain text - it arrives with half-written tags and asterisks
      // that are not formatting - but it reveals the same way an answer does.
      this.appendReveal(this.textBuf, joined, performance.now())
    } else {
      this.textBuf.appendChild(document.createTextNode(joined))
    }
    if (this.textKind === 'reasoning' && this.textBuf.classList.contains('clamp')
        && !this.textBuf.classList.contains('expanded')) {
      this.textBuf.scrollTop = this.textBuf.scrollHeight
    }
    this.scrollIfNeeded()
  }

  /**
   * Render the streaming answer.
   *
   * `force` is used at turn end, where correctness matters more than cost and a
   * single full markdown pass is cheap.
   */
  private renderLive(force: boolean) {
    if (!this.liveEl) return
    if (force) {
      if (this.liveTimer) {
        clearTimeout(this.liveTimer)
        this.liveTimer = 0
      }
      this.liveEl.classList.remove('streaming')
      this.liveEl.innerHTML = this.live.finish()
      this.livePainted = this.live.text.length
      return
    }
    const now = performance.now()
    if (now - this.lastLiveRender < LIVE_RENDER_MS) {
      // Do not drop the update; re-render when the throttle window closes.
      if (!this.liveTimer) {
        this.liveTimer = window.setTimeout(() => {
          this.liveTimer = 0
          this.renderLive(false)
        }, LIVE_RENDER_MS)
      }
      return
    }
    this.lastLiveRender = now
    const src = this.live.text
    const delta = src.slice(this.livePainted)
    if (!delta) return
    this.livePainted = src.length
    this.appendReveal(this.liveEl, delta, now)
  }

  /**
   * Put an arrival on screen as words, in nodes that are never touched again.
   *
   * This replaces a design that re-rendered the growing paragraph and kept the
   * newest spans separate until they could be "folded" into the settled text.
   * Folding is a rewrite: the finished words left the animation's compositing
   * layer and were re-rastered, so text blinked again roughly half a second
   * after arriving - the "playing catch up" the blur was blamed for. Anything
   * that rewrites settled text will do that, however carefully the rewrite is
   * guarded, because the guard has to compare markup it has to regenerate.
   *
   * So nothing here is ever rewritten. The stream is appended as spans, one per
   * word, each animated once with a small stagger; markdown arrives exactly once,
   * at stream end, as a single pass that replaces the whole block.
   */
  private appendReveal(host: HTMLElement, delta: string, now: number) {
    const words = revealWords(delta)
    if (!revealOn() || !words.length) {
      host.appendChild(document.createTextNode(delta))
      return
    }
    const gap = this.lastPaintAt ? now - this.lastPaintAt : LIVE_RENDER_MS
    this.lastPaintAt = now
    const inst = gap > 0 ? (words.length * 1000) / gap : 0
    this.wordRate = this.wordRate ? this.wordRate * 0.7 + inst * 0.3 : inst
    host.style.setProperty('--tin-dur', `${revealDuration(this.wordRate)}ms`)
    const frag = document.createDocumentFragment()
    for (let i = 0; i < words.length; i++) {
      const span = document.createElement('span')
      span.className = 'tin'
      span.textContent = words[i]
      // Stagger inside one arrival so a burst of six words reads as six words
      // arriving, not one block fading in. Capped: a burst that arrives after a
      // stall must not finish animating after the model has already moved on.
      const delay = Math.min(i * 24, 168)
      if (delay) span.style.animationDelay = `${delay}ms`
      frag.append(span)
    }
    host.append(frag)
  }

  /** Streaming blocks render as plain text with newlines kept; see appendReveal. */
  private beginLive(el: HTMLElement) {
    this.liveEl = el
    this.live = new LiveMarkdown()
    this.livePainted = 0
    this.lastLiveRender = 0
    this.lastPaintAt = 0
    el.classList.add('streaming')
  }
  private push(s: string, kind: BlockKind) {
    if (this.textKind !== kind || !this.textBuf) {
      // Leaving an answer block: finish its markdown before anything else is
      // appended, so a tool call in the middle of an answer cannot truncate the
      // formatting of the part already on screen.
      if (this.textKind === 'answer') this.finalizeAnswer()
      if (this.textKind === 'reasoning') this.closeReasoning()
      const label = kind === 'reasoning' ? 'thinking' : kind === 'user' ? 'you' : 'cwoffee'
      this.textBuf = this.block(kind, label)
      this.textKind = kind
      if (kind === 'reasoning') {
        this.textBuf.classList.add('clamp', 'streaming')
        this.textBuf.dataset.thinkStart = String(performance.now())
        this.reasoningEls.push(this.textBuf)
      }
      if (kind === 'answer') {
        this.answerEls.push(this.textBuf)
        this.beginLive(this.textBuf)
      }
      this.setLive(this.textBuf)
    }
    this.pending.push(s)
    if (!this.raf) this.raf = requestAnimationFrame(() => this.flush())
  }

  /**
   * Follow the bottom unless the user scrolled up to read something. Fighting a
   * reader's scroll is the most annoying thing a streaming chat can do.
   */
  private scrollIfNeeded() {
    const sc = this.root.closest('.scroller') as HTMLElement | null
    if (!sc) return
    const away = sc.scrollHeight - sc.scrollTop - sc.clientHeight
    if (away < 160) sc.scrollTop = sc.scrollHeight
  }

  // ---- event application -------------------------------------------------

  apply(ev: EngineEvent) {
    switch (ev.kind) {
      case 'turnStart': {
        this.flushNow()
        this.turn = null
        this.textBuf = null
        this.textKind = null
        this.collected = { answer: '', reasoning: '' }
        this.answerEls = []
        const head = this.el('turn-head', `turn ${ev.d.turn_id} · ${ev.d.model}`)
        this.ensureTurn()
        this.turn!.insertBefore(head, this.turn!.firstChild)
        break
      }
      case 'text':
        this.collected.answer += ev.d
        this.push(ev.d, 'answer')
        break
      case 'reasoning':
        this.collected.reasoning += ev.d
        this.push(ev.d, 'reasoning')
        break
      case 'toolCallBegin':
        // A tool call ends the reasoning that preceded it: the model has decided,
        // so that block should say how long it took now, not at turn end.
        if (this.textKind === 'reasoning') this.closeReasoning()
        this.openTool(ev.d.id, ev.d.name)
        break
      case 'toolCallReady':
        this.setToolArgs(ev.d.id, ev.d.args)
        break
      case 'toolCallParseError':
        this.toolFail(ev.d.id, `arguments unparseable: ${ev.d.message}\nraw: ${ev.d.raw}`)
        break
      case 'toolExecOutput':
        this.appendToolOut(ev.d.id, ev.d.stream, ev.d.chunk)
        break
      case 'toolExecEnd':
        this.closeTool(ev.d.id, ev.d.ok, ev.d.exit_code)
        this.noteChanges(ev.d.id, ev.d.changes)
        break
      case 'approvalRequested':
        // rendered by the composer overlay, not inline: it needs the keyboard
        break
      case 'note':
        this.flushNow()
        this.block('note', 'note').textContent = ev.d.message
        break
      case 'error':
        this.flushNow()
        this.block(ev.d.fatal ? 'fail' : 'warn', ev.d.fatal ? 'error' : 'warning').textContent = ev.d.message
        break
      case 'turnEnd':
        this.flushNow()
        this.finalizeAnswer()
        this.settleReasoning()
        this.setLive(null)
        this.textBuf = null
        this.textKind = null
        // Actions after the answer, not before the files: the answer is what you
        // read, the files are what it produced, and copying is a thing you do to
        // a finished turn rather than one still arriving.
        this.appendTurnActions()
        break
      case 'usage':
      case 'toolCallArgs':
      case 'toolExecStart':
        break
    }
  }

  /** Flush any buffered deltas. Call before reading textContent or ending a turn. */
  flushNow() {
    if (this.raf) cancelAnimationFrame(this.raf)
    this.raf = 0
    if (this.liveTimer) {
      clearTimeout(this.liveTimer)
      this.liveTimer = 0
    }
    this.flush()
  }

  /**
   * Final full markdown pass over the streaming answer.
   *
   * During streaming only the tail block is re-parsed; at the end one pass over
   * the whole source guarantees the displayed result matches what
   * `renderMarkdown` would produce for the stored text.
   */
  private finalizeAnswer() {
    if (this.liveTimer) {
      clearTimeout(this.liveTimer)
      this.liveTimer = 0
    }
    if (this.liveEl) {
      if (this.live.text) this.renderLive(true)
      this.liveEl = null
    }
    this.live = new LiveMarkdown()
  }

  // ---- user echo ----------------------------------------------------------

  /**
   * The engine does not echo the user's message over the event stream, so the
   * UI must render it itself or your own turns silently vanish.
   */
  addUser(text: string, attachments: AttachmentMeta[] = []) {
    const body = this.block('user', 'you')
    this.textBuf = body
    this.textKind = 'user'
    body.textContent = text
    this.appendAttachmentTags(body, attachments)
  }

  /**
   * Name and size, not a thumbnail. History pages carry attachment metadata
   * only, so a thumbnail here would disappear on reopen and make the transcript
   * look lossy when the payload is in fact intact.
   */
  private appendAttachmentTags(body: HTMLElement, attachments: AttachmentMeta[]) {
    if (!attachments.length) return
    const row = this.el('attlist')
    for (const a of attachments) {
      const kb = Math.max(1, Math.round(a.bytes / 1024))
      row.append(this.el('atttag', `${a.name ?? a.mime} · ${kb} kB`))
    }
    body.append(row)
  }

  /** Surface a shell-level failure in the transcript, not just the log. */
  /** A quiet informational line: migration notices, backend defaults. */
  showNote(msg: string) {
    const el = document.createElement('div')
    el.className = 'blk blk-note'
    const body = document.createElement('div')
    body.className = 'blk-body'
    body.textContent = msg
    el.appendChild(body)
    // ensureTurn, not this.turn!: a startup note can arrive before any turn
    // exists, and the first version of this dereferenced a null turn.
    this.ensureTurn()
    this.turn!.appendChild(el)
    this.scrollIfNeeded()
  }

  showError(msg: string) {
    this.flushNow()
    this.block('fail', 'error').textContent = msg
  }

  // ---- produced files -----------------------------------------------------

  /**
   * Files a turn wrote into the workspace, shown as clickable attachments.
   * The path is workspace-relative and the shell re-validates it before
   * opening anything, so a model-authored name cannot escape the workspace.
   */
  /**
   * Show the files this turn has produced.
   *
   * The event carries the *cumulative* list and fires whenever a tool produces
   * something, so appending a row per event used to print the same file two and
   * three times under one answer. One row per turn, keyed by path: a file that
   * is written and then edited is one attachment, showing the latest size.
   */
  /**
   * Show a question the model asked, and park the card until it is answered.
   *
   * The turn is genuinely stopped while this is up, so the card is the whole
   * point of the screen: the question first, the choices one click away, and a
   * typed answer offered alongside them. The recommended option is badged rather
   * than preselected - the model's opinion is information, not a default the
   * user has to notice and undo.
   */
  showQuestion(
    q: QuestionRequested,
    onAnswer: (r: { text: string; options: number[] }) => void,
  ) {
    this.flushNow()
    this.ensureTurn()
    const card = this.el('blk blk-ask')
    const head = this.el('blk-head')
    head.append(this.el('blk-label', 'asking'))
    // `.blk` is a two-column grid (label column, content column). Appending the
    // question, the options and the buttons straight onto the card put them in
    // *column order*, so the option list landed in the 74 px label gutter and
    // every sentence wrapped at five characters. One wrapper, spanning both
    // columns, keeps the content in one continuous width.
    const body = this.el('ask-body')
    card.append(head, body)
    body.append(this.el('ask-q', q.question))

    // Selection is provisional until Send. Clicking an option used to answer the
    // model on the spot, which made a misclick an answer - and on a question
    // about deleting something that is a real cost.
    const sel = new Set<number>()
    const opts = this.el('ask-opts')
    opts.setAttribute('role', q.multi ? 'group' : 'radiogroup')
    const rows: HTMLElement[] = []

    q.options.forEach((o, i) => {
      const row = this.el('ask-opt')
      row.setAttribute('role', q.multi ? 'checkbox' : 'radio')
      row.setAttribute('aria-checked', 'false')
      row.tabIndex = 0
      if (q.multi) row.classList.add('is-multi')
      row.append(this.el('ask-mark', q.multi ? '☐' : '○'))

      const main = this.el('ask-opt-main')
      // The recommendation belongs with the option it describes: on its own line
      // it read like a caption for the whole list.
      const title = this.el('ask-opt-title')
      title.append(this.el('ask-lbl', o.label))
      if (q.recommended === i) {
        // A text node, not just the pill: without the gap the accessible name of
        // the row reads "Markdownrecommended".
        title.append(document.createTextNode(' '))
        title.append(this.el('ask-badge', 'recommended'))
        row.classList.add('is-rec')
      }
      main.append(title)
      if (o.detail) main.append(this.el('ask-detail', o.detail))
      row.append(main)
      rows.push(row)
      opts.append(row)
    })
    body.append(opts)

    let input: HTMLInputElement | null = null
    if (q.allow_custom) {
      const row = this.el('ask-custom')
      input = document.createElement('input')
      input.className = 'ask-input'
      input.placeholder = q.multi
        ? 'or type your own answer (it is sent with any boxes you tick)'
        : 'or type your own answer instead'
      row.append(input)
      body.append(row)
    }

    const actions = this.el('ask-actions')
    const send = document.createElement('button')
    send.type = 'button'
    send.className = 'ask-submit'
    send.textContent = 'Send answer'
    send.disabled = true
    const decline = document.createElement('button')
    decline.type = 'button'
    decline.className = 'ask-decline'
    decline.textContent = 'Decline'
    actions.append(send, decline)
    body.append(actions)

    const paint = () => {
      rows.forEach((row, i) => {
        const on = sel.has(i)
        row.classList.toggle('is-sel', on)
        row.setAttribute('aria-checked', on ? 'true' : 'false')
        const mark = row.querySelector('.ask-mark')
        if (mark) mark.textContent = q.multi ? (sel.has(i) ? '☑' : '☐') : sel.has(i) ? '●' : '○'
      })
      send.disabled = sel.size === 0 && !(input?.value.trim() ?? '')
      send.textContent = q.multi && sel.size > 1 ? `Send ${sel.size} answers` : 'Send answer'
    }

    const finish = (options: number[], text: string) => {
      if (card.classList.contains('answered')) return
      card.classList.add('answered')
      // Say what was actually sent, so a reload or a second look answers
      // "what did I pick?".
      const labels = options.map((i) => q.options[i]?.label ?? `#${i + 1}`).join(' + ')
      const shown = [labels, text].filter((x) => x && (!labels || x !== text))
      body.append(
        this.el(
          'ask-answered',
          options.length ? `sent: ${[labels, text].filter((x) => x && x !== labels).join(' · ') || labels}` : text ? `sent: ${text}` : shown.length ? `sent: ${shown.join(' · ')}` : 'declined',
        ),
      )
      card.querySelectorAll('.ask-opt, .ask-input, .ask-submit, .ask-decline').forEach((n) => {
        n.classList.add('is-done')
        if (n instanceof HTMLInputElement || n instanceof HTMLButtonElement) n.disabled = true
      })
      onAnswer({ text, options })
    }

    const submit = () => {
      const picked = [...sel].sort((a, b) => a - b)
      const typed = (input?.value ?? '').trim()
      const labels = picked.map((i) => q.options[i].label)
      // With both a pick and typed words, the typed words go in `text`: the
      // engine already has the picks separately, and it reports text as "User
      // also said", which is what a constraint added beside a choice is.
      const text = picked.length ? (typed && typed !== labels.join(', ') ? typed : labels.join(', ')) : typed
      if (!picked.length && !text) return
      finish(picked, text)
    }

    rows.forEach((row, i) => {
      const toggle = () => {
        if (card.classList.contains('answered')) return
        if (q.multi) {
          if (sel.has(i)) sel.delete(i)
          else sel.add(i)
        } else {
          sel.clear()
          sel.add(i)
        }
        paint()
      }
      row.addEventListener('click', toggle)
      row.addEventListener('keydown', (e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault()
          toggle()
        }
      })
    })
    if (input) {
      input.addEventListener('input', paint)
      input.addEventListener('keydown', (e) => {
        if (e.key === 'Enter') {
          e.preventDefault()
          submit()
        }
      })
    }
    send.addEventListener('click', submit)
    // Declining is an explicit button, not a guess at what Esc meant. It sends an
    // empty answer, which the tool reports as "dismissed without answering".
    decline.addEventListener('click', () => finish([], ''))

    paint()
    // The build above is all detached. Without this the card exists, is wired
    // up, answers correctly when driven by a test that reaches it by id - and is
    // never on screen. A rewrite of this method lost exactly these three lines,
    // and the self-test caught it as "question card did not render".
    this.turn!.appendChild(card)
    this.questionCards.set(q.id, card)
    this.scrollIfNeeded()
  }

  /** Close a question card answered elsewhere - another tab, another device. */
  settleQuestion(id: number) {
    const card = this.questionCards.get(id)
    if (!card || card.classList.contains('answered')) return
    card.classList.add('answered', 'by-other')
    card.append(this.el('ask-answered', 'answered elsewhere'))
  }

  /**
   * Close a question that closed *without* an answer. The engine says so when its
   * tool stops waiting: the reply channel is gone, so clicking Send from here
   * aims an answer at a call that has already returned, and the turn has moved on
   * having been told to decide for itself. Saying "no answer in time" is the
   * difference between a dead button and an explained one.
   */
  expireQuestion(id: number) {
    const card = this.questionCards.get(id)
    if (!card || card.classList.contains('answered') || card.classList.contains('expired')) return
    card.classList.add('expired')
    card.append(
      this.el('ask-expired', 'no answer in time — the model was told to decide for itself'),
    )
    card
      .querySelectorAll('.ask-opt, .ask-input, .ask-submit, .ask-decline')
      .forEach((n) => {
        n.classList.add('is-done')
        if (n instanceof HTMLInputElement || n instanceof HTMLButtonElement) n.disabled = true
      })
    card.querySelectorAll('.ask-opt').forEach((n) => (n as HTMLElement).setAttribute('aria-disabled', 'true'))
  }

  addFiles(files: OutputFile[]) {
    if (!files.length) return
    this.flushNow()
    this.ensureTurn()
    for (const f of files) {
      const at = this.turnFiles.findIndex((e) => e.path === f.path)
      if (at >= 0) this.turnFiles[at] = f
      else this.turnFiles.push(f)
    }
    this.fileStrip?.remove()
    this.fileStrip = this.fileRow(this.turnFiles)
    this.turn!.appendChild(this.fileStrip)
    this.scrollIfNeeded()
  }

  private fileRow(files: OutputFile[]): HTMLElement {
    const row = this.el('filestrip')
    for (const f of files) {
      const chip = document.createElement('button')
      chip.className = 'filechip'
      chip.type = 'button'
      chip.dataset.path = f.path
      chip.title = `Open ${f.path}`
      const ico = this.el('file-ico')
      ico.append(fileIcon(f.mime))
      chip.append(ico)
      const meta = this.el('file-meta')
      meta.append(this.el('file-name', f.name))
      meta.append(this.el('file-sub', `${shortMime(f.mime)} · ${fmtSize(f.bytes)}`))
      chip.append(meta)
      row.append(chip)
    }
    return row
  }

  /**
   * The line under a finished turn: copy what it said, export the conversation as
   * a Markdown document, and a branch button that is honest about not existing
   * yet. A disabled control that says why is better than an absent feature the
   * user has to guess at, and better than a live button that does nothing.
   */
  private turnActionsRow(answerText: string, session: number): HTMLElement {
    const row = this.el('turn-acts')

    const btn = (cls: string, label: string, title: string, icon: SVGElement) => {
      const b = document.createElement('button')
      b.type = 'button'
      b.className = `tact ${cls}`
      b.title = title
      b.setAttribute('aria-label', label)
      const ico = this.el('tact-ico')
      ico.append(icon)
      b.append(ico, this.el('tact-label', label))
      row.append(b)
      return b
    }

    const copy = btn('copy', 'Copy', 'Copy this answer as Markdown', iconCopy())
    copy.onclick = () => {
      void copyToClipboard(answerText).then((ok) => {
        if (this.onToast) this.onToast(ok ? 'copied the answer' : 'the webview refused the clipboard', !ok)
        if (ok) {
          copy.classList.add('done')
          setTimeout(() => copy.classList.remove('done'), 1200)
        }
      })
    }

    const share = btn('share', 'Share as Markdown', 'Write this conversation to exports/ in its workspace', iconDoc())
    share.onclick = () => {
      if (!this.onExport) {
        if (this.onToast) this.onToast('export is not available here', true)
        return
      }
      share.disabled = true
      void this.onExport(session)
        .then((f) => {
          // The chip is the receipt: it is the same affordance every other
          // generated file gets, and it proves the write happened where it says.
          this.addFiles([f])
          this.appendTurnActions()
          if (this.onToast) this.onToast(`saved to ${f.path}`)
        })
        .catch((e) => {
          if (this.onToast) this.onToast(`export failed: ${e}`, true)
        })
        .finally(() => {
          share.disabled = false
        })
    }

    const branch = btn('branch', 'Branch', 'Branch this conversation - not built yet', iconBranch())
    branch.disabled = true
    return row
  }

  /** Live turns get their actions at turn end, from the text we streamed. */
  private appendTurnActions() {
    if (!this.turn) return
    this.turn.querySelector('.turn-acts')?.remove()
    const text = this.turnAnswer.join('')
    if (!text.trim() && !this.turnFiles.length) return
    this.turn.appendChild(this.turnActionsRow(text, this.sessionId))
  }

  // ---- tool cards ---------------------------------------------------------

  /**
   * Build a tool card. One builder for the live path and the history path:
   * the two used to be written out separately, which is how the history view
   * ended up with a card that could not collapse.
   *
   * Collapsed, a card is its header line and nothing else - the same shape a
   * thinking block has. The old version clamped the arguments and output to "3.5
   * lines" instead, so a collapsed transcript was a wall of half-visible JSON and
   * the header, which was the only part worth reading, got lost in it.
   */
  private buildToolCard(name: string) {
    const card = this.el('blk blk-tool')
    card.dataset.tool = name
    const head = this.el('blk-head')
    const headline = this.el('tool-name', toolHeadline(name, 'run'))
    head.append(headline)
    const subject = this.el('tool-subject')
    head.append(subject)
    // The change count sits between the subject and the status so a scan of a
    // long list of cards answers "did this touch anything?" without opening one.
    const stat = this.el('tool-stat')
    // Empty while the call runs. "Running a command · running" says it twice, and
    // the pulsing dot plus the peeking body already carry the state; the chip is
    // for the verdict, which only exists afterwards.
    const status = this.el('tool-status')
    head.append(stat, status)
    const args = this.el('tool-args')
    const out = this.el('tool-out')
    const detail = this.el('tool-detail clamp')
    detail.append(args, out)
    card.append(head, detail)

    // Collapsed: the whole card is the target. Expanded: only the label, so
    // selecting output text does not hide it.
    card.addEventListener('click', (e) => {
      const isHead = (e.target as HTMLElement)?.closest?.('.blk-head')
      if (detail.classList.contains('expanded')) {
        if (!isHead) return
        detail.classList.remove('expanded')
        head.classList.remove('open')
        card.classList.remove('is-open')
        detail.scrollTop = 0
      } else {
        detail.classList.add('expanded')
        head.classList.add('open')
        card.classList.add('is-open')
      }
    })
    return { card, head, args, out, detail, subject, stat }
  }

  private openTool(id: string, name: string) {
    // ask_user renders as the question card, not as a tool card. Two cards for
    // one question - "ask_user running" next to the actual question - reads as
    // a bug, and the second one carries no information.
    if (name === 'ask_user') return
    this.flushNow()
    this.textBuf = null
    this.textKind = null
    this.ensureTurn()
    const { card, head, args, out, detail, subject, stat } = this.buildToolCard(name)
    const status = head.querySelector('.tool-status') as HTMLElement
    this.setLive(card)
    this.turn!.appendChild(card)
    this.tools.set(id, {
      card,
      args,
      out,
      status,
      detail,
      subject,
      stat,
      name,
      stdout: '',
      stderr: '',
    })
    this.scrollIfNeeded()
  }

  private refs(id: string): ToolRefs | undefined {
    return this.tools.get(id)
  }

  private setToolArgs(id: string, args: unknown) {
    const t = this.refs(id)
    if (!t) return  // ask_user has no card; its question card carries the text
    t.args.textContent = typeof args === 'string' ? args : JSON.stringify(args)
    const subject = subjectFor(t.name, args)
    if (subject) {
      t.subject.textContent = subject
      t.subject.title = subject
    }
  }

  private appendToolOut(id: string, stream: ExecStream, chunk: string) {
    const t = this.refs(id)
    if (!t) return
    if (stream === 'stderr') t.stderr += chunk
    else t.stdout += chunk
    // Clamp what the DOM holds; a `yes` loop must not grow the page forever.
    const shown = (t.stdout + t.stderr).slice(-8000)
    t.out.textContent = shown
    // While collapsed and still running, pin the clamp to the tail: the newest
    // line of a long build log is the one worth seeing. Same rule as thinking.
    if (!t.detail.classList.contains('expanded')) {
      t.detail.scrollTop = t.detail.scrollHeight
    }
    this.scrollIfNeeded()
  }

  /**
   * Put `+N -M` on the card, and the diff inside it.
   *
   * The counts come from the engine comparing what was on disk a moment before
   * the call with what is on disk after, not from anything the model claimed.
   * The diff is UI metadata: it deliberately never enters the prompt, because a
   * diff of a file the model just wrote is the most expensive possible thing to
   * put in front of it, and the prompt bytes are what the prefix cache keys on.
   */
  private noteChanges(id: string, changes: FileChange[]) {
    const t = this.refs(id)
    if (t) this.noteChangesInto(t, changes)
  }

  private noteChangesInto(
    t: { stat: HTMLElement; detail: HTMLElement; card: HTMLElement },
    changes: FileChange[],
  ) {
    if (!changes?.length) return
    const added = changes.reduce((n, c) => n + c.added, 0)
    const removed = changes.reduce((n, c) => n + c.removed, 0)
    if (added || removed) {
      const created = changes.filter((c) => c.created)
      t.stat.textContent = created.length === changes.length && !removed
        ? `+${added}`
        : `+${added} -${removed}`
      t.stat.className = `tool-stat ${removed ? 'mix' : 'add'}`
      t.stat.title = changes
        .map((c) => `${c.path}: ${c.created ? 'new file' : `+${c.added} -${c.removed}`}`)
        .join('\n')
      t.card.classList.add('has-changes')
    }
    let diffShown = false
    for (const c of changes) {
      if (!c.diff) continue
      diffShown = true
      const pre = this.el('tool-diff')
      if (changes.length > 1) pre.append(this.el('diff-path', c.path))
      for (const line of c.diff.split('\n')) {
        if (!line) continue
        const cls = line.startsWith('@@')
          ? 'diff-hunk'
          : line.startsWith('+++') || line.startsWith('---')
            ? 'diff-file'
            : line.startsWith('+')
              ? 'diff-add'
              : line.startsWith('-')
                ? 'diff-del'
                : 'diff-ctx'
        pre.append(this.el(`diff-line ${cls}`, line))
      }
      t.detail.append(pre)
    }
    if (diffShown) {
      // The diff *is* the arguments, restated as what actually changed. Keeping
      // the raw call JSON above it means reading the same edit twice, once as a
      // 400-character blob and once in a form a person can scan.
      t.card.classList.add('has-diff')
    }
  }

  private closeTool(id: string, ok: boolean, exit: number | null) {
    const t = this.refs(id)
    if (!t) return
    // Past tense, because by now it has happened.
    const label = t.card.querySelector('.tool-name')
    if (label) label.textContent = toolHeadline(t.name, 'done')
    t.status.textContent = ok
      ? exit === null || exit === 0
        ? 'OK'
        : `failed · exit ${exit}`
      : `failed${exit === null ? '' : ` · exit ${exit}`}`
    t.status.className = `tool-status ${ok ? 'ok' : 'bad'}`
    t.card.classList.add(ok ? 'tool-ok' : 'tool-bad')
    // A finished card collapses to its header: the transcript a user scans is a
    // list of what happened, not of how it happened.
    if (!t.detail.classList.contains('expanded')) t.detail.scrollTop = 0
  }

  private toolFail(id: string, msg: string) {
    const t = this.refs(id)
    if (!t) {
      this.block('fail', 'error').textContent = msg
      return
    }
    const label = t.card.querySelector('.tool-name')
    if (label) label.textContent = toolHeadline(t.name, 'bad')
    t.status.textContent = 'arguments unparseable'
    t.status.className = 'tool-status bad'
    t.out.textContent = msg
  }

  // ---- history ------------------------------------------------------------

  /** Dev diagnostic: did the markdown pass actually run, and what did it emit? */
  debugLastAnswerHtml(limit = 220): string {
    const els = this.root.querySelectorAll('.blk-answer .blk-body')
    const last = els[els.length - 1] as HTMLElement | undefined
    return last ? last.innerHTML.slice(0, limit) : ''
  }

  /** Re-render a stored session. Called on switch, not during streaming. */
  /** The scroll container the transcript lives in, for anchoring on prepend. */
  scrollEl(): HTMLElement | null {
    return this.root.closest('.scroller') as HTMLElement | null
  }

  loadHistory(msgs: StoredMessage[]) {
    this.clear()
    this.root.appendChild(this.historyFragment(msgs))
    pageLog(`history: loaded ${msgs.length} messages`)
  }

  /**
   * Insert an older page above what is already shown.
   *
   * Returns the new scroll height so the caller can keep the viewport anchored:
   * prepending shifts everything down, and without an adjustment the reader jumps
   * to the top of the newly loaded page instead of staying where they were.
   */
  prependHistory(msgs: StoredMessage[]) {
    const before = this.root.scrollHeight
    const frag = this.historyFragment(msgs)
    this.root.insertBefore(frag, this.root.firstChild)
    return { shift: this.root.scrollHeight - before }
  }

  /**
   * Render stored messages into a detached fragment.
   *
   * Detached matters: prepending requires the blocks to exist before they are in
   * the document, and building them against `this.turn` would attach them to the
   * live turn group instead.
   */
  private historyFragment(msgs: StoredMessage[]): DocumentFragment {
    const frag = document.createDocumentFragment()
    // Tool results are separate rows; match them to their card by id rather than
    // "the most recent card", which is wrong across page boundaries.
    const cards = new Map<string, { out: HTMLElement; stat: HTMLElement; detail: HTMLElement; card: HTMLElement }>()
    let lastCard: { out: HTMLElement; stat: HTMLElement; detail: HTMLElement; card: HTMLElement } | undefined

    let lastWrap: HTMLElement | undefined
    const blk = (kind: string, label: string) => {
      const wrap = this.el(`blk blk-${kind}`)
      const head = this.el('blk-head', label)
      const body = this.el('blk-body')
      wrap.append(head, body)
      frag.appendChild(wrap)
      lastWrap = wrap
      return body
    }

    for (const m of msgs) {
      if (m.role === 'user') {
        const body = blk('user', 'you')
        body.textContent = m.content
        this.appendAttachmentTags(body, m.attachments ?? [])
      } else if (m.role === 'assistant') {
        if (m.reasoning) {
          const r = blk('reasoning', m.reasoning_ms ? fmtThink(m.reasoning_ms) : 'thinking')
          r.classList.add('clamp', 'settled')
          r.textContent = m.reasoning
        }
        const body = blk('answer', 'cwoffee')
        body.innerHTML = renderMarkdown(m.content)
        // The source, kept on the element: right-click "Copy" and any future
        // export from the DOM should hand back what the model wrote, not the
        // rendering of it.
        body.dataset.md = m.content
        // Same affordance as a live turn. In history the copy is that one message
        // and the share is the whole conversation, which is what the buttons say.
        const acts = this.turnActionsRow(m.content, this.sessionId)
        acts.classList.add('from-history')
        frag.appendChild(acts)
        if (m.files?.length) {
          // Into the fragment, directly under the answer that produced them. This
          // used to call ensureTurn() and append to the live turn, which landed the
          // chips at the *top* of a reloaded transcript, above every message: a
          // session reopened after writing a file looked like it had no attachment,
          // which is exactly the "the file was not shown at turn end" report.
          const strip = this.fileRow(m.files)
          frag.insertBefore(strip, lastWrap ? lastWrap.nextSibling : null)
        }
        for (const call of parseCalls(m.tool_calls_json)) {
          if (call.name === 'ask_user') {
            // No tool card, live or in history: a card headed `ASK_USER` names the
            // mechanism and hides the only interesting part, which is the question
            // the agent actually asked. The answer (the tool result) lands in the
            // same block, so the exchange reads as one exchange.
            const body = blk('note', 'asked you')
            let q = ''
            try {
              const a = JSON.parse(call.arguments)
              if (a && typeof a.question === 'string') q = a.question
            } catch {
              /* half-streamed arguments; fall back to the generic line */
            }
            // Two children, not one: the tool result is written into `out`, and
            // sharing a single text node would overwrite the question with the
            // answer, which loses the half of the exchange worth keeping.
            const qEl = this.el('ask-hist-q')
            qEl.textContent = q || 'the agent asked you something and kept the answer to itself'
            const ansEl = this.el('ask-hist-a')
            body.append(qEl, ansEl)
            const wrap = body.closest('.blk') as HTMLElement
            const ref = { out: ansEl, stat: ansEl, detail: body, card: wrap }
            cards.set(call.id, ref)
            lastCard = ref
            continue
          }
          const { card, head, args, out, detail, subject, stat } = this.buildToolCard(call.name)
          // Stored calls have finished, so they read in the past tense; the status
          // chip stays empty because a stored tool result does not record whether
          // the call succeeded.
          const label = head.querySelector('.tool-name')
          if (label) label.textContent = toolHeadline(call.name, 'done')
          ;(head.querySelector('.tool-status') as HTMLElement).textContent = '' 
          frag.appendChild(card)
          args.textContent = call.arguments
          const subj = subjectFor(call.name, call.arguments)
          if (subj) {
            subject.textContent = subj
            subject.title = subj
          }
          const ref = { out, stat, detail, card }
          cards.set(call.id, ref)
          lastCard = ref
          this.tools.set(call.id, {
            card,
            args,
            out,
            detail,
            subject,
            stat,
            name: call.name,
            status: head.querySelector('.tool-status') as HTMLElement,
            stdout: '',
            stderr: '',
          })
        }
      } else if (m.role === 'tool') {
        const ref = (m.tool_call_id && cards.get(m.tool_call_id)) || lastCard
        if (ref) {
          ref.out.textContent = m.content.slice(0, 8000)
          // Counts and diff are stored with the result, so a reloaded session
          // shows the same `+N -M` it showed live.
          if (m.changes?.length) this.noteChangesInto(ref, m.changes)
        }
      }
    }
    return frag
  }
}

/** Minimum interval between live markdown passes. */
const LIVE_RENDER_MS = 120

function parseCalls(jsonStr: string): Array<{ id: string; name: string; arguments: string }> {
  if (!jsonStr) return []
  try {
    const v = JSON.parse(jsonStr)
    return Array.isArray(v) ? v : []
  } catch {
    return []
  }
}

/**
 * What a finished or running tool call says about itself, in English.
 *
 * `write_file` is a name from the schema; what the user wants to know is that a
 * file was written, and whether it is still going. The tense carries the state:
 * "Writing a file" is live, "Wrote a file" is done, "Could not write a file" is a
 * call whose arguments never parsed.
 */
const TOOL_HEAD: Record<string, { run: string; done: string; bad: string }> = {
  write_file: { run: 'Writing a file', done: 'Wrote a file', bad: 'Could not write a file' },
  edit_file: { run: 'Editing a file', done: 'Edited a file', bad: 'Could not edit that file' },
  read_file: { run: 'Reading a file', done: 'Read a file', bad: 'Could not read that file' },
  write_xlsx: { run: 'Writing a spreadsheet', done: 'Wrote a spreadsheet', bad: 'Could not write the spreadsheet' },
  download_file: { run: 'Downloading a file', done: 'Downloaded a file', bad: 'Could not download that file' },
  web_fetch: { run: 'Opening a page', done: 'Opened a page', bad: 'Could not open that page' },
  web_search: { run: 'Searching the web', done: 'Searched the web', bad: 'The search did not parse' },
  exec: { run: 'Running a command', done: 'Ran a command', bad: 'The command did not parse' },
  ask_user: { run: 'Asked you something', done: 'Asked you something', bad: 'Asked you something' },
}

export function toolHeadline(name: string, tense: 'run' | 'done' | 'bad'): string {
  const h = TOOL_HEAD[name]
  if (h) return h[tense]
  return tense === 'done' ? 'Done' : tense === 'bad' ? 'Failed' : 'Working'
}

/**
 * The same label for an approval prompt. An unrecognised tool keeps its raw id:
 * a name the harness has never seen is itself the information a decision turns on,
 * and "Working" would hide it.
 */
export function riskHeadline(name: string): string {
  return TOOL_HEAD[name] ? toolHeadline(name, 'run') : name
}

/**
 * What a tool call is about, shown in its header.
 *
 * "write_file · ok" tells you nothing you care about; which file changed is the
 * one fact you want without expanding the card. `exec` is deliberately left
 * out: its argument is a whole command, and putting it here would just move the
 * noise into the header and make every row resize.
 */
const SUBJECT_KEYS = ['path', 'file', 'url', 'query']

export function subjectFor(name: string, raw: unknown): string {
  if (name === 'exec') return ''
  if (typeof raw === 'string') {
    // Tool arguments arrive as a stream, so this is often a fragment. Match a
    // *closed* string value only: a half-written path would resize the header
    // on every token and look like a rendering fault.
    for (const k of SUBJECT_KEYS) {
      const m = new RegExp(`"${k}"\\s*:\\s*"((?:[^"\\\\]|\\\\.)*)"`).exec(raw)
      if (m) {
        try {
          return JSON.parse(`"${m[1]}"`) as string
        } catch {
          return m[1]
        }
      }
    }
    return ''
  }
  if (!raw || typeof raw !== 'object') return ''
  const obj = raw as Record<string, unknown>
  for (const k of SUBJECT_KEYS) {
    const v = obj[k]
    if (typeof v === 'string' && v.trim()) return v
  }
  return ''
}
