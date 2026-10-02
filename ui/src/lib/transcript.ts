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
import { renderMarkdown } from './markdown'
import { pageLog } from './pagelog'

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

  constructor(root: HTMLElement) {
    this.root = root
    // A link that navigates replaces the whole app with a web page, which is
    // worse than a dead link. Opening in the external browser needs Tauri's
    // opener plugin (not wired yet); until then links are inert.
    root.addEventListener('click', (e) => {
      const a = (e.target as HTMLElement | null)?.closest?.('a')
      if (a) {
        e.preventDefault()
        pageLog(`link suppressed: ${a.getAttribute('href')}`)
      }
    })
  }

  clear() {
    this.root.textContent = ''
    this.turn = null
    this.textBuf = null
    this.tools.clear()
    this.answerEls = []
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
    this.textBuf.appendChild(document.createTextNode(joined))
    this.scrollIfNeeded()
  }

  private push(s: string, kind: BlockKind) {
    if (this.textKind !== kind || !this.textBuf) {
      const label = kind === 'reasoning' ? 'thinking' : kind === 'user' ? 'you' : 'cowry'
      this.textBuf = this.block(kind, label)
      this.textKind = kind
      if (kind === 'answer') this.answerEls.push(this.textBuf)
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
        this.renderAnswers()
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
    this.flush()
  }

  /**
   * One markdown pass per answer block, at stream end. This is the only place
   * innerHTML is ever assigned, and `renderMarkdown` escapes first so the only
   * tags in the result are ones it wrote.
   */
  private renderAnswers() {
    for (const el of this.answerEls) {
      const text = el.textContent ?? ''
      if (!text) continue
      el.innerHTML = renderMarkdown(text)
    }
    this.answerEls = []
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

function parseCalls(jsonStr: string): Array<{ id: string; name: string; arguments: string }> {
  if (!jsonStr) return []
  try {
    const v = JSON.parse(jsonStr)
    return Array.isArray(v) ? v : []
  } catch {
    return []
  }
}
