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

import type { EngineEvent, ExecStream, StoredMessage } from './events'
import { renderMarkdown, LiveMarkdown } from './markdown'

export type BlockKind = 'user' | 'reasoning' | 'answer' | 'tool' | 'note' | 'warn' | 'fail'

interface ToolRefs {
  card: HTMLElement
  args: HTMLElement
  out: HTMLElement
  status: HTMLElement
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
  /**
   * Answer bodies created this turn. Markdown is applied to them exactly once,
   * at stream end — never per delta.
   */
  private answerEls: HTMLElement[] = []
  // Live markdown state for the answer block currently streaming.
  private liveEl: HTMLElement | null = null
  private live = new LiveMarkdown()
  private lastLiveRender = 0
  private liveTimer = 0

  constructor(root: HTMLElement) {
    this.root = root
    // A link that navigates replaces the whole app with a web page, which is
    // worse than a dead link, so default navigation is always prevented. The URL
    // then goes to Tauri's opener plugin, which checks it against the declared
    // scope. Outside Tauri (the vite preview) there is no opener, so the link
    // stays inert rather than hijacking the window.
    root.addEventListener('click', (e) => {
      const a = (e.target as HTMLElement | null)?.closest?.('a')
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
    this.live = new LiveMarkdown()
  }

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

  private flush() {
    this.raf = 0
    if (!this.pending.length) return
    const joined = this.pending.join('')
    this.pending = []
    if (!this.textBuf) return
    if (this.textKind === 'answer') {
      // Answers format as they arrive. The naive version re-parses the whole
      // answer on every frame, which is what makes streaming UIs stutter on a
      // local model; instead only the still-growing tail block is re-parsed and
      // finished blocks are rendered exactly once.
      this.live.push(joined)
      this.renderLive(false)
    } else {
      this.textBuf.appendChild(document.createTextNode(joined))
    }
    this.scrollIfNeeded()
  }

  /**
   * Render the streaming answer.
   *
   * `force` is used at turn end, where correctness matters more than cost and a
   * single full pass is cheap.
   */
  private renderLive(force: boolean) {
    if (!this.liveEl) return
    const now = performance.now()
    if (!force && now - this.lastLiveRender < LIVE_RENDER_MS) {
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
    this.liveEl.innerHTML = this.live.render()
  }

  /** Start a fresh live-render target. */
  private beginLive(el: HTMLElement) {
    this.liveEl = el
    this.live = new LiveMarkdown()
    this.lastLiveRender = 0
  }

  private push(s: string, kind: BlockKind) {
    if (this.textKind !== kind || !this.textBuf) {
      // Leaving an answer block: finish its markdown before anything else is
      // appended, so a tool call in the middle of an answer cannot truncate the
      // formatting of the part already on screen.
      if (this.textKind === 'answer') this.finalizeAnswer()
      const label = kind === 'reasoning' ? 'thinking' : kind === 'user' ? 'you' : 'cowry'
      this.textBuf = this.block(kind, label)
      this.textKind = kind
      if (kind === 'answer') {
        this.answerEls.push(this.textBuf)
        this.beginLive(this.textBuf)
      }
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
        break
      case 'approvalRequested':
        // rendered by the composer overlay, not inline: it needs the keyboard
        break
      case 'error':
        this.flushNow()
        this.block(ev.d.fatal ? 'fail' : 'warn', ev.d.fatal ? 'error' : 'warning').textContent = ev.d.message
        break
      case 'turnEnd':
        this.flushNow()
        this.finalizeAnswer()
        this.textBuf = null
        this.textKind = null
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
      if (this.live.text) this.liveEl.innerHTML = this.live.finish()
      this.liveEl = null
    }
    this.live = new LiveMarkdown()
  }

  // ---- user echo ----------------------------------------------------------

  /**
   * The engine does not echo the user's message over the event stream, so the
   * UI must render it itself or your own turns silently vanish.
   */
  addUser(text: string) {
    this.textBuf = this.block('user', 'you')
    this.textKind = 'user'
    this.textBuf.textContent = text
  }

  // ---- tool cards ---------------------------------------------------------

  private openTool(id: string, name: string) {
    this.flushNow()
    this.textBuf = null
    this.textKind = null
    this.ensureTurn()
    const card = this.el('blk blk-tool')
    const head = this.el('blk-head')
    head.append(this.el('tool-name', name))
    const status = this.el('tool-status', 'running…')
    head.appendChild(status)
    const args = this.el('tool-args')
    const out = this.el('tool-out')
    card.append(head, args, out)
    this.turn!.appendChild(card)
    this.tools.set(id, { card, args, out, status, stdout: '', stderr: '' })
    this.scrollIfNeeded()
  }

  private refs(id: string): ToolRefs | undefined {
    return this.tools.get(id)
  }

  private setToolArgs(id: string, args: unknown) {
    const t = this.refs(id)
    if (!t) return
    t.args.textContent = typeof args === 'string' ? args : JSON.stringify(args)
  }

  private appendToolOut(id: string, stream: ExecStream, chunk: string) {
    const t = this.refs(id)
    if (!t) return
    if (stream === 'stderr') t.stderr += chunk
    else t.stdout += chunk
    // Clamp what the DOM holds; a `yes` loop must not grow the page forever.
    const shown = (t.stdout + t.stderr).slice(-8000)
    t.out.textContent = shown
    this.scrollIfNeeded()
  }

  private closeTool(id: string, ok: boolean, exit: number | null) {
    const t = this.refs(id)
    if (!t) return
    t.status.textContent = ok ? `ok${exit === null ? '' : ` · exit ${exit}`}` : `failed${exit === null ? '' : ` · exit ${exit}`}`
    t.status.className = `tool-status ${ok ? 'ok' : 'bad'}`
    t.card.classList.add(ok ? 'tool-ok' : 'tool-bad')
  }

  private toolFail(id: string, msg: string) {
    const t = this.refs(id)
    if (!t) {
      this.block('fail', 'error').textContent = msg
      return
    }
    t.status.textContent = 'unparseable'
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
    const cards = new Map<string, { out: HTMLElement }>()
    let lastCard: { out: HTMLElement } | undefined

    const blk = (kind: string, label: string) => {
      const wrap = this.el(`blk blk-${kind}`)
      const head = this.el('blk-head', label)
      const body = this.el('blk-body')
      wrap.append(head, body)
      frag.appendChild(wrap)
      return body
    }

    for (const m of msgs) {
      if (m.role === 'user') {
        blk('user', 'you').textContent = m.content
      } else if (m.role === 'assistant') {
        if (m.reasoning) blk('reasoning', 'thinking').textContent = m.reasoning
        const body = blk('answer', 'cowry')
        body.innerHTML = renderMarkdown(m.content)
        for (const call of parseCalls(m.tool_calls_json)) {
          const card = this.el('blk blk-tool')
          const head = this.el('blk-head')
          head.append(this.el('tool-name', call.name))
          head.append(this.el('tool-status', ''))
          const args = this.el('tool-args')
          const out = this.el('tool-out')
          card.append(head, args, out)
          frag.appendChild(card)
          args.textContent = call.arguments
          const ref = { out }
          cards.set(call.id, ref)
          lastCard = ref
          this.tools.set(call.id, {
            card,
            args,
            out,
            status: head.lastElementChild as HTMLElement,
            stdout: '',
            stderr: '',
          })
        }
      } else if (m.role === 'tool') {
        const ref = (m.tool_call_id && cards.get(m.tool_call_id)) || lastCard
        if (ref) ref.out.textContent = m.content.slice(0, 8000)
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
