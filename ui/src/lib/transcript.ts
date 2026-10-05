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

import type { AttachmentMeta, EngineEvent, ExecStream, StoredMessage } from './events'
import { renderMarkdown, LiveMarkdown, type InlineReveal } from './markdown'
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

function fileGlyph(mime: string): string {
  if (mime.startsWith('image/')) return '\u{1f5bc}'
  if (mime === 'application/pdf') return '\u{1f4c4}'
  if (mime.includes('spreadsheet') || mime.endsWith('csv')) return '\u{1f4ca}'
  if (mime.includes('wordprocessing') || mime === 'text/markdown' || mime === 'text/plain')
    return '\u{1f4c4}'
  return '\u{1f4c1}'
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
  // Nodes owned by the reveal: not rewritten while they are animating.
  private revealFrozen: HTMLElement | null = null
  private revealPara: HTMLElement | null = null
  private revealHead: HTMLElement | null = null
  private revealNodes = new Map<number, HTMLElement>()
  private lastLiveRender = 0
  private liveTimer = 0

  /** Set by the shell: opening a file is a Tauri concern, not a renderer one. */
  onOpenFile: ((path: string) => void) | null = null

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
  }

  /**
   * Mark the one block that is currently producing output. The indicator is on
   * the latest active block only - several pulsing at once reads as noise - and
   * it is removed outright at turn end rather than left idle, so a settled
   * transcript carries no animation at all.
   */
  private setLive(body: HTMLElement | null) {
    const wrap = body?.parentElement ?? null
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
      // Answers format as they arrive. The naive version re-parses the whole
      // answer on every frame, which is what makes streaming UIs stutter on a
      // local model; instead only the still-growing tail block is re-parsed and
      // finished blocks are rendered exactly once.
      this.live.push(joined)
      this.renderLive(false)
    } else {
      this.textBuf.appendChild(document.createTextNode(joined))
    if (this.textKind === 'reasoning' && this.textBuf.classList.contains('clamp')
        && !this.textBuf.classList.contains('expanded')) {
      this.textBuf.scrollTop = this.textBuf.scrollHeight
    }
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
    const r = this.live.renderReveal()
    if (!r || r.mode === 'block') {
      const html = r ? r.html : ''
      this.dropRevealNodes()
      if (this.liveEl.innerHTML !== html) this.liveEl.innerHTML = html
      return
    }
    this.paintReveal(r)
  }

  /**
   * Streaming paint with the newest arrivals kept as their own nodes.
   *
   * The reveal animation needs ~320ms per chunk and the paint cadence is 120ms,
   * so anything rewritten on each paint is replaced before it can finish. Only
   * the static head is rewritten; each chunk span is created once, animates
   * once, and is removed when its text has been folded into the head.
   */
  private paintReveal(r: InlineReveal) {
    const els = this.ensureRevealNodes()
    if (els.frozenHost.dataset.src !== r.frozen) {
      els.frozenHost.dataset.src = r.frozen
      els.frozenHost.innerHTML = r.frozen
    }

    const live = new Set(r.chunks.map((c) => c.id))
    for (const [id, el] of this.revealNodes) {
      if (live.has(id)) continue
      // Fold, do not rewrite. The chunk's finished nodes move into the settled
      // run; the rest of the paragraph is left exactly as it is.
      while (el.firstChild) els.head.append(el.firstChild)
      el.remove()
      this.revealNodes.delete(id)
    }

    // Re-parsing the settled run on every paint was the flash. Rewriting
    // innerHTML tears down and re-rasters text that did not change, and the
    // folded text simultaneously leaves the animation's compositing layer - so
    // the newest words visibly blinked once more, ~520ms after arriving.
    // Accumulating nodes instead means this only fires when the inline markup
    // genuinely changes: a `**bold**` that straddled two chunks.
    if (els.head.innerHTML !== r.head) els.head.innerHTML = r.head

    for (const c of r.chunks) {
      if (this.revealNodes.has(c.id)) continue
      const el = document.createElement('span')
      el.className = 'tin'
      el.innerHTML = c.html
      els.para.append(el)
      this.revealNodes.set(c.id, el)
    }
  }

  /** Finished blocks, then one paragraph holding `<head><chunk spans>`. */
  private ensureRevealNodes() {
    if (this.revealPara?.isConnected && this.revealHead?.isConnected) {
      return {
        frozenHost: this.revealFrozen as HTMLElement,
        para: this.revealPara,
        head: this.revealHead,
      }
    }
    this.liveEl!.innerHTML = ''
    const frozenHost = document.createElement('div')
    frozenHost.className = 'tin-frozen'
    const para = document.createElement('p')
    para.className = 'tin-para'
    const head = document.createElement('span')
    head.className = 'tin-head'
    para.append(head)
    this.liveEl!.append(frozenHost, para)
    this.revealFrozen = frozenHost
    this.revealPara = para
    this.revealHead = head
    this.revealNodes = new Map()
    return { frozenHost, para, head }
  }

  private dropRevealNodes() {
    this.revealFrozen = null
    this.revealPara = null
    this.revealHead = null
    this.revealNodes = new Map()
  }

  /** Start a fresh live-render target. */
  private beginLive(el: HTMLElement) {
    this.liveEl = el
    this.live = new LiveMarkdown()
    this.lastLiveRender = 0
    this.dropRevealNodes()
  }

  private push(s: string, kind: BlockKind) {
    if (this.textKind !== kind || !this.textBuf) {
      // Leaving an answer block: finish its markdown before anything else is
      // appended, so a tool call in the middle of an answer cannot truncate the
      // formatting of the part already on screen.
      if (this.textKind === 'answer') this.finalizeAnswer()
      const label = kind === 'reasoning' ? 'thinking' : kind === 'user' ? 'you' : 'cwoffee'
      this.textBuf = this.block(kind, label)
      this.textKind = kind
      if (kind === 'reasoning') {
        this.textBuf.classList.add('clamp')
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
    this.dropRevealNodes()
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
    onAnswer: (r: { text: string; option: number | null }) => void,
  ) {
    this.flushNow()
    this.ensureTurn()
    const card = this.el('blk blk-ask')
    const head = this.el('blk-head')
    head.append(this.el('blk-label', 'asking'))
    card.append(head)
    card.append(this.el('ask-q', q.question))

    const opts = this.el('ask-opts')
    q.options.forEach((o, i) => {
      const b = this.el('ask-opt')
      b.setAttribute('role', 'button')
      b.tabIndex = 0
      if (q.recommended === i) {
        b.classList.add('is-rec')
        b.append(this.el('ask-badge', 'recommended'))
      }
      b.append(this.el('ask-lbl', o.label))
      if (o.detail) b.append(this.el('ask-detail', o.detail))
      const pick = () => answer(i, o.label)
      b.addEventListener('click', pick)
      b.addEventListener('keydown', (e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault()
          pick()
        }
      })
      opts.append(b)
    })
    card.append(opts)

    if (q.allow_custom) {
      const row = this.el('ask-custom')
      const input = document.createElement('input')
      input.className = 'ask-input'
      input.placeholder = 'or type your own answer'
      input.addEventListener('keydown', (e) => {
        if (e.key === 'Enter' && input.value.trim()) answer(null, input.value.trim())
      })
      row.append(input)
      card.append(row)
    }

    const answer = (option: number | null, text: string) => {
      if (card.classList.contains('answered')) return
      // The click is the answer; a second one must not send a second reply.
      card.classList.add('answered')
      card.append(this.el('ask-answered', `answered: ${text}`))
      onAnswer({ text, option })
    }

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
      chip.append(this.el('file-ico', fileGlyph(f.mime)))
      const meta = this.el('file-meta')
      meta.append(this.el('file-name', f.name))
      meta.append(this.el('file-sub', `${shortMime(f.mime)} · ${fmtSize(f.bytes)}`))
      chip.append(meta)
      row.append(chip)
    }
    return row
  }

  // ---- tool cards ---------------------------------------------------------

  /**
   * Build a tool card. One builder for the live path and the history path:
   * the two used to be written out separately, which is how the history view
   * ended up with a card that could not collapse.
   *
   * Tool output collapses the same way a thinking block does - collapsed by
   * default, click anywhere on the collapsed card to expand, click the label to
   * collapse again. Most calls are one or two lines, so the collapsed height is
   * chosen to show those in full and only clamp the long ones.
   */
  private buildToolCard(name: string) {
    const card = this.el('blk blk-tool')
    const head = this.el('blk-head')
    head.append(this.el('tool-name', name))
    const subject = this.el('tool-subject')
    head.append(subject)
    const status = this.el('tool-status', 'running…')
    head.appendChild(status)
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
    return { card, head, args, out, detail, subject }
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
    const { card, head, args, out, detail, subject } = this.buildToolCard(name)
    const status = head.lastElementChild as HTMLElement
    this.setLive(card)
    this.turn!.appendChild(card)
    this.tools.set(id, {
      card,
      args,
      out,
      status,
      detail,
      subject,
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

  private closeTool(id: string, ok: boolean, exit: number | null) {
    const t = this.refs(id)
    if (!t) return
    t.status.textContent = ok ? `ok${exit === null ? '' : ` · exit ${exit}`}` : `failed${exit === null ? '' : ` · exit ${exit}`}`
    t.status.className = `tool-status ${ok ? 'ok' : 'bad'}`
    t.card.classList.add(ok ? 'tool-ok' : 'tool-bad')
    // Snap back to the start once it is done, so a scanned card begins where a
    // reader expects rather than mid-log.
    if (!t.detail.classList.contains('expanded')) t.detail.scrollTop = 0
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
        if (m.files?.length) {
          this.ensureTurn()
          this.turn!.appendChild(this.fileRow(m.files))
        }
        for (const call of parseCalls(m.tool_calls_json)) {
          const { card, head, args, out, detail, subject } = this.buildToolCard(call.name)
          ;(head.lastElementChild as HTMLElement).textContent = ''
          frag.appendChild(card)
          args.textContent = call.arguments
          const subj = subjectFor(call.name, call.arguments)
          if (subj) {
            subject.textContent = subj
            subject.title = subj
          }
          const ref = { out }
          cards.set(call.id, ref)
          lastCard = ref
          this.tools.set(call.id, {
            card,
            args,
            out,
            detail,
            subject,
            name: call.name,
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
