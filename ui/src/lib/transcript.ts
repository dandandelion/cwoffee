import { pageLog } from './pagelog'
import { LoopHold, IDLE_HOLD, prefersStill } from './anipause'

/**
 * The transcript's cup: two drawings, one element. The loop that runs while a block is producing
 * output, the loop that runs when it is resting, and a still of each for reduced motion.
 */
const CUP_ART = {
  busy: '/cwoffee_generating_generation_1.webp',
  busyStill: '/cwoffee_generating_generation_still.webp',
  idle: '/cwoffee_generating_idle_1.webp',
  idleStill: '/cwoffee_generating_idle_still.webp',
}
/** The pause between laps of the resting cup: the same range the opening-screen cup draws from. */
const CUP_IDLE_HOLD = IDLE_HOLD

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
import { renderMarkdown, LiveMarkdown, needsMarkdown, firstBlockEnd, firstItemEnd, listMarker, maybeListMarker, tableHeadEnd, tableHeadPending, tableRowEnd, tableSkeletonHtml, tableRowHtml, fenceOpen, fenceStart, codeHeadHtml, blockKind, foldsWhenClosed, actIcon, lineGutterHtml, lineGutterDelta, lineNumbers, revealOn, revealWords, revealDuration, REVEAL_ANIM_MS, InlineStream } from './markdown'
import { fmtThink, fmtWords, countWords } from './think'
import { LoopWatch, describeLoop, type LoopFinding } from './loop'
import { openPreview, inlineSvg, inlineRender } from './preview'

/**
 * Clipboard feedback without a toast system: the button says it, briefly.
 *
 * It says it by changing the glyph, not the text. This used to write the word "copied" into the
 * button, and the button is a 24px icon box: the word was clipped to a couple of letters, and on
 * restoring, `prev` was the button's old `textContent` - empty, because its content is an inline
 * SVG - so the icon came back as nothing and the button went on being clickable while invisible.
 * A tick in the same slot, with the saved markup put back afterwards, is the same message at the
 * size the row has.
 */
function markCopied(btn: HTMLElement, icon: string) {
  btn.innerHTML = actIcon('check', 14)
  btn.classList.add('done')
  window.setTimeout(() => {
    btn.innerHTML = icon
    btn.classList.remove('done')
  }, 1400)
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
 * Replace one finished reveal span with the text it wraps.
 *
 * Returns 1 when the span went away and 0 when it stayed: a span that holds markup —
 * a link, a code span, a bold run, a hard break — is the markup, and collapsing that
 * would lose the thing the word *is* rather than the thing the animation was.
 */
function collapseSpan(s: HTMLElement): number {
  if (s.firstElementChild) return 0
  const kids = s.childNodes
  if (kids.length === 1 && kids[0].nodeType === Node.TEXT_NODE) {
    s.replaceWith(document.createTextNode(kids[0].nodeValue ?? ''))
    return 1
  }
  if (!kids.length) {
    s.remove()
    return 1
  }
  return 0
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

/**
 * What kind of growth just happened, which is what decides whether the view follows it. See
 * `Transcript.follow`.
 */
export type Growth = 'work' | 'ask' | 'text' | 'chrome' | 'you'


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
  /**
   * The run accordion the next thinking entry or tool call belongs to.
   *
   * Consecutive thinking and tool calls share one accordion. What ends it is the
   * model's own message text: a run is "everything it did between two things it
   * said". That is the division the reader actually experiences - a turn that made
   * nine tool calls used to print nine cards, each its own expander, and the
   * answer you came for was below all of them.
   */
  private runGroup: HTMLElement | null = null
  /** Whether the entries inside a run carry their contents. The app's setting, not this
      screen's: see `setVerbose`. */
  private verbose = false
  private tools = new Map<string, ToolRefs>()
  private pending: string[] = []
  private raf = 0
  /** Collected during a turn so it can be committed once, at stream end. */
  private collected: { answer: string; reasoning: string } = { answer: '', reasoning: '' }
  // What the thinking blocks of this turn left on screen: one header line each, rewritten from
  // "thinking" to the duration when the thought ends. There is no body to clamp or scroll - see
  // `thoughtLine`.
  private reasoningEls: HTMLElement[] = []
  /**
   * The loop detector for the turn being streamed, and whether its finding has been shown yet.
   * Live only: a session reloaded from disk is not replayed through it, because a turn that already
   * finished is not going to start looping again, so paying for the check on load would be paying
   * for nothing. The cost is measured in `scripts/loop-probe.mjs`: about 12µs per 18-character
   * delta, which is about 0.2ms of a 16.7ms frame at a typical token rate, on a 400-word ring with
   * a map capped at 800 entries - so it does not grow with the length of the turn.
   */
  private loop: LoopWatch | null = null
  private loopShown = false
  /** Set by the app, so "this looks stuck" can be said out loud once per turn. */
  onLoop?: (f: LoopFinding) => void
  /** The thought line currently streaming, and how much thinking has landed behind it. */
  private thoughtBuf: HTMLElement | null = null
  private thoughtWords = 0
  /**
   * Answer bodies created this turn. Markdown is applied to them exactly once,
   * at stream end — never per delta.
   */
  private answerEls: HTMLElement[] = []
  // Live markdown state for the answer block currently streaming.
  private liveEl: HTMLElement | null = null
  /**
   * The `<ul>`/`<ol>` for a list that is being written, and whether the element
   * now being filled was built as final structure rather than a placeholder.
   *
   * Building a list live is the only way four bullets can appear one at a time:
   * the parser's own rule is that a list ends at a blank line, so waiting for the
   * block means painting four `-` lines as text and then replacing them with four
   * `<li>` boxes in one go. `tailStructured` is how `closeBlock` knows to leave
   * those nodes alone: re-rendering a list that is already a list is a rewrite of
   * settled text, and every rewrite of settled text has been a flicker report.
   */
  private liveList: HTMLElement | null = null
  private tailStructured = false
  /**
   * The block element whose text is currently being written - the `<p>` or the
   * `<li>`. Distinct from `inlineHost`, which is where the next word goes and may
   * be a `<strong>` or `<code>` opened inside it.
   *
   * Testing the wrong one is how bold broke in real use: a construct opened in one
   * paint left `inlineHost` pointing at the `<strong>`, the next paint compared the
   * `<li>` against it, decided the host was "not live", and appended the rest of
   * the item - closing marker and all - as raw text. Word-for-word what the
   * screenshots showed.
   */
  private liveTextHost: HTMLElement | null = null
  /**
   * The `<tbody>` of a table that is still being written, and the delimiter row
   * that fixed its columns. Built as soon as the header and delimiter lines are
   * down - the point at which the shape is known - so each row the model types
   * lands as a `<tr>` instead of the whole table replacing a paragraph at the end.
   */
  private liveTable: { delim: string; tbody: HTMLElement } | null = null
  /**
   * The fenced block being written, when the model is inside one right now.
   *
   * A code fence is the one block a reader can watch being built, and until now what they watched
   * was backticks and raw lines rather than a block. So the head lands the moment the opening fence
   * does - language, line count, actions, all of it disabled - and the code streams into the
   * `<code>` that is already on screen. The block still *commits* only when the closing fence
   * lands: rendering early is a display decision, committing early would re-parse text that has not
   * arrived yet, and `firstBlockEnd` keeps that half of the rule.
   */
  private liveCode: {
    wrap: HTMLElement
    code: HTMLElement
    lines: number
    meta: HTMLElement | null
    gutter: HTMLElement | null
  } | null = null
  /** Files produced so far this turn, deduped by path, and the one row that
   *  shows them. */
  private turnFiles: OutputFile[] = []
  /**
   * The last few announcements about the app itself, with the conversation each
   * one was about. Kept across repaints of that conversation, dropped when the
   * memory overflows - oldest first.
   */
  private notes: { text: string; session: number | null }[] = []
  private questionCards = new Map<number, HTMLElement>()
  /** A turn's file line is one or two elements: the attachments on their own right-aligned line,
   *  and the working strip underneath. Tracked together because both belong to the turn, and a
   *  leftover element re-parents yesterday's files into today's turn. */
  private fileEls: HTMLElement[] = []
  private live = new LiveMarkdown()
  /**
   * How much of `live` has been turned into nodes. The streaming paint is
   * append-only: this only ever moves forward, and the only thing it decides is
   * which characters have not been put on screen yet.
   */
  private livePainted = 0
  /**
   * The answer streams one *block element* at a time.
   *
   * Text goes into the `<p>` that is going to be that paragraph, and when the
   * blank line lands the element is finished **in place**: a paragraph with no
   * markup keeps its spans, its height and its position, because a `<p>` full of
   * word spans is already what the parser would have produced. That is what keeps
   * the layout of everything already on screen perfectly still while the model
   * writes - committing used to swap spans for a `<p>`, which changed the box by
   * a line's worth of blank space and nudged the whole transcript.
   */
  private liveTail: HTMLElement | null = null
  /**
   * Inline emphasis for the paragraph currently being written: the stream that
   * decides where a `<strong>` starts, and the element words are currently being
   * appended into (the paragraph itself when nothing is open).
   */
  private inline = new InlineStream()
  private inlineHost: HTMLElement | null = null
  /** Source offset where the block currently being written begins. */
  private blockStart = 0
  /** Following the stream: pin to the bottom unless the reader scrolled away. */
  private pinned = true
  private programmatic = false
  /**
   * The position the app wrote with its own scroll, so a scroll event can be attributed.
   *
   * A boolean flag is not enough, and the bug it caused was the user's: the flag is cleared on the
   * next frame, and a wheel notch or a drag that lands inside that one-frame window is mistaken for
   * the app's own scroll. The reader's gesture is dropped, `pinned` stays true, and the view they
   * just moved is yanked back down - which is what my own harness caught by scrolling the page by
   * hand. So the event is only attributed to us when the position actually matches what we wrote.
   */
  private wroteTop = -1
  private followRaf = 0
  private followVel = 0
  private scHooked: HTMLElement | null = null
  /**
   * Words per second, smoothed. The reveal duration comes from this so the
   * animation is the length of the gap it is filling rather than a constant that
   * a fast model outruns.
   */
  private wordRate = 0
  private lastPaintAt = 0
  private lastLiveRender = 0
  private liveTimer = 0
  /**
   * Collapse a finished turn's per-word reveal spans back into text. On by default;
   * the off switch exists for the measurement that proves the collapse is what makes
   * a long session scrollable (`scripts/perf-rig.mjs --collapse off`).
   */
  collapseSettled = true

  /** Set by the shell: opening a file is a Tauri concern, not a renderer one. */
  onOpenFile: ((path: string) => void) | null = null
  /**
   * Set by the shell. Export is a backend operation - the file has to land in the
   * workspace under the same containment rules as every other generated file - so
   * the transcript asks rather than doing it, and gets an OutputFile back to show.
   */
  /**
   * "Share as Markdown": build the document and let the *person* say where it
   * goes. Resolves to one sentence for the toast ("saved to …", "downloaded …").
   *
   * It used to return an `OutputFile` that the transcript turned into a chip.
   * That put a share in the same row as the model's own files, and the next turn
   * then treated the export as something it had produced.
   */
  onShare: ((session: number) => Promise<string>) | null = null
  /**
   * "Save" on an attachment. The transcript has no filesystem and no idea which
   * transport it is on, so it asks: the desktop answers with a save dialog, a browser
   * tab by pulling the bytes over the wire and downloading them. Resolves to one
   * sentence for the toast ("saved to …", "downloaded …"), or rejects with the reason.
   *
   * Only attachments get this control, because only attachments are *meant* to be
   * handed over: the model wrote them into `.cws/assistant_attachments/` on purpose.
   * Everything else a turn produces is working material, and offering to save a
   * `probe.py` is how a chat log turns into a pile of the model's leftovers.
   */
  onDeliver: ((path: string) => Promise<string | null>) | null = null

  /**
   * Where an app-level note goes now that it is no longer a block in the transcript.
   *
   * Notes used to be painted into the stream between the turns: a migration notice, a
   * compaction, "the endpoint defaults to X". They were never part of an answer, and they
   * landed next to the answer anyway, so a reader scanning a conversation could not tell
   * what the model had said from what the software had muttered. They are still worth
   * showing — that is what this hook is for — but in the chrome, where they can be
   * dismissed, and out of the record of what was said.
   */
  onNote: ((text: string, session: number | null) => void) | null = null
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
        // A block whose fence has not closed has no working buttons. The browser already refuses
        // real clicks on a disabled control; this refuses the synthetic ones a harness (or a helper
        // that dispatches events) can aim at it, so "the buttons wait for the fence" is true of
        // every way to press them.
        if (blk.classList.contains('streaming') || act.matches(':disabled')) return
        const kind = blk.dataset.kind ?? blockKind(blk.dataset.lang ?? '')
        const src = code.textContent ?? ''
        switch (act.dataset.act) {
          case 'copy': {
            const ico = act.innerHTML
            // The tick is a receipt, not a promise. It arrives once the clipboard has the text - the
            // reader's eye is already on this corner of the row, so it does not also need a toast -
            // and a refusal says so where the reader can act on it.
            void (navigator.clipboard?.writeText(src) ?? Promise.reject(new Error('no clipboard')))
              .then(() => markCopied(act, ico))
              .catch((err) => {
                pageLog(`copy failed: ${err}`)
                this.onToast?.('the webview refused the clipboard', true)
              })
            break
          }
          case 'preview':
            if (kind === 'html' || kind === 'svg') openPreview(kind, src)
            break
          case 'render': {
            // The eye answers "what does this source make?" without leaving the paragraph the
            // reader is in. Folded-by-default blocks are useless without it: the source is hidden,
            // so the render is the thing that should be one press away.
            const on = inlineRender(blk, src)
            act.setAttribute('aria-pressed', String(on))
            act.title = on ? 'Hide the rendered block' : 'Show the rendered block'
            // A folded block that just gained a render is showing something; one that lost it is
            // showing nothing but a title, so it opens back up rather than going blank.
            if (!on && blk.classList.contains('folded')) {
              blk.classList.remove('folded')
              const f = blk.querySelector('.code-fold') as HTMLElement | null
              if (f) {
                f.setAttribute('aria-expanded', 'true')
                f.title = 'Collapse this block'
              }
            }
            break
          }
          case 'inline':
            inlineSvg(blk, src)
            break
          case 'fold': {
            const folded = blk.classList.toggle('folded')
            act.setAttribute('aria-expanded', String(!folded))
            act.title = folded ? 'Expand this block' : 'Collapse this block'
            break
          }
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
      // A thought is a line and nothing else: there is no body to open, so nothing here expands.
      // The old branch toggled a `.blk-body` that streamed the model's reasoning - see
      // `thoughtLine` for why it is gone.
      if (t?.closest?.('.blk-reasoning')) return
      const a = t?.closest?.('a')
      if (!a) return
      e.preventDefault()
      const href = a.getAttribute('href') ?? ''
      void openExternal(href)
    })
  }

  clear() {
    this.stopFollow()
    this.stopCup()
    this.root.textContent = ''
    this.turn = null
    this.runGroup = null
    this.textBuf = null
    this.loop = null
    this.loopShown = false
    this.tools.clear()
    this.answerEls = []
    this.liveEl = null
    this.setLive(null)
    this.live = new LiveMarkdown()
    this.turnFiles = []
    this.fileEls = []
    this.turnAnswer = []
    this.livePainted = 0
    this.liveTail = null
    this.inlineHost = null
    this.liveTextHost = null
    this.liveList = null
    this.liveTable = null
    this.liveCode = null
    this.tailStructured = false
    this.blockStart = 0
    this.wordRate = 0
    this.lastPaintAt = 0
    // `textContent = ''` detached the cup with everything else. Put it back and let it hide itself:
    // an empty transcript sits under the opening screen's own, bigger cup, and two of the same
    // drawing on one page reads as a bug whether or not either of them moves.
    this.placeCup()
  }

  /**
   * Mark the one block that is currently producing output, and with it the state of the
   * transcript's cup: the generating loop while output moves, the resting loop once it stops.
   * Nothing else in the transcript animates on its own - the old pulsing dot was removed
   * everywhere, in the gutter and beside every card inside the folded group - so the cup is the
   * whole "is it working?" tell, and it is the same element either way, only a different drawing.
   */
  private setLive(body: HTMLElement | null) {
    // A tool card is its own block, so its marker belongs on the card. Taking the
    // parent put `live` on the turn, which matched no CSS: a running command had no
    // indicator at all while a running answer did.
    const wrap = body?.classList?.contains('blk') ? body : (body?.parentElement ?? null)
    if (this.liveWrap === wrap) return
    const wasBusy = !!this.liveWrap
    this.liveWrap?.classList.remove('live')
    this.liveWrap = wrap
    wrap?.classList.add('live')
    // The name row still carries the turn's state - CSS and the checks read `.live` - and the flip
    // is what changes the transcript's cup: the generating loop while a block produces output, the
    // resting loop with its random holds once none does.
    const marker = (this.turn?.querySelector(':scope > .turn-who-cwoffee') ?? null) as HTMLElement | null
    marker?.classList.toggle('live', !!body)
    if (wasBusy !== !!body) this.repaintCup()
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

  /** A real button, because a clickable div is a guess at what the user meant. */
  private btn(cls: string, text: string): HTMLButtonElement {
    const b = document.createElement('button')
    b.type = 'button'
    b.className = cls
    b.textContent = text
    return b
  }

  /**
   * The one name marker a turn carries for cwoffee, hung in the gutter and stuck to the top
   * of the scroller while that turn's text is being read.
   *
   * Every said block used to repeat the name down the left edge: a turn with a run and four
   * paragraphs said "CWOFFEE" five times, and the gutter turned into a column of identical
   * words. One per turn, floating, is the same affordance `.code-head` uses for a code block's
   * buttons — you always know whose text you are in the middle of, without being told five
   * times. The marker is a zero-height sticky row so the name costs no vertical space; the word
   * itself hangs in the gutter where the labels already live, so it never nudges the text
   * column. Yours is not here: your label sits with your bubble, which is where it is read.
   */
  private whoCwoffee(): HTMLElement {
    this.ensureTurn()
    const found = this.turn!.querySelector(':scope > .turn-who-cwoffee') as HTMLElement | null
    if (found) return found
    const wrap = this.el('turn-who turn-who-cwoffee')
    wrap.append(this.el('who-name', 'cwoffee'))
    this.turn!.appendChild(wrap)
    return wrap
  }

  // ---- the transcript's cup ---------------------------------------------------------------
  // One picture for the whole transcript, and one place for it to sit: the bottom band of the
  // transcript pane, reserved by the scroller's padding, where nothing else is drawn. It used to
  // hang under each turn's name (a column of small cups), then became the last element in the
  // content (one cup that the whole turn pushed around). The band is the third shape and the first
  // one where the picture does not move: it is still the artwork at the bottom of the conversation,
  // still exactly one of them, and now it stays put while lines arrive.
  // One picture for the whole transcript, moved to the end of whatever it contains, rather than one
  // under each turn's name. Under the name it multiplied - a session of short turns became a column
  // of small cups, which is the same turn-per-label mistake the name itself already had - and a cup
  // small enough to fit a 58px gutter is too small to read as a drawing at all. At the end of the
  // content there is exactly one, big enough to be the artwork, and turn end puts it under the
  // Copy / Share / Branch row because that row belongs to the turn while this belongs to the page.
  //
  // It is never created twice: `cup()` makes the element once and `placeCup()` moves it. That is
  // also what the earlier per-turn rule was working around - an animated raster decodes its own
  // frame stream per element, so a cup per turn costs a decode per turn, and a loaded session used
  // to get stills because of it. Which is why a loaded session's cup looked frozen.

  private cupImg: HTMLImageElement | null = null
  private cupHold: LoopHold | null = null
  private cupVisible = true
  /** How far above the end of the content the reader may be and still be shown the cup. The cup is
   *  pinned to the pane, so anything deeper into the transcript than this is text that would sit
   *  under a 136px drawing. The band is 168px deep and the drawing plus its seat is 144px, so the
   *  picture stops clearing the content at almost exactly that distance - the number is geometry,
   *  not taste. */
  private static readonly CUP_REST_PX = 24
  /** How long the reader has to be away from the end before the picture is put away. The follow's
   *  own catch-up leaves the view 20-60px short of the bottom for a frame or two at a time; a cup
   *  that vanishes and returns inside half a second is a blink, and this one is 136px wide. */
  private static readonly CUP_HIDE_MS = 400
  private cupHideTimer = 0

  /** The cup element, made once per transcript and re-used for as long as this transcript lives. */
  private cup(): HTMLImageElement {
    if (!this.cupImg) {
      const img = document.createElement('img')
      img.className = 'transcript-cup'
      img.alt = ''
      img.draggable = false
      img.setAttribute('src', this.cupSrc())
      this.cupImg = img
    }
    return this.cupImg
  }

  /** Which of the four files this cup should be showing right now. */
  private cupSrc(): string {
    const busy = !!this.liveWrap
    const still = prefersStill()
    if (still) return busy ? CUP_ART.busyStill : CUP_ART.idleStill
    return busy ? CUP_ART.busy : CUP_ART.idle
  }

  /**
   * Move the cup after whatever was last added, and hide it when the transcript holds nothing: the
   * opening screen has its own, larger cup, and a page showing the same drawing twice is a bug
   * whether or not it is animated.
   *
   * The picture lives in the pane, not in the page that scrolls. The scroller's bottom padding
   * reserves the band it sits in (see `.transcript-cup` and `--cup-band` in app.css), so the newest
   * line of text stops above it and the drawing never travels. Appending it to `this.root` - the
   * old shape, the last thing in the content - made the picture's on-screen position a function of
   * every arriving line and every follow step: measured in `ui/perf/probe.ts` over three streamed
   * turns that was 1,348px of travel, p95 261px in a single frame, and six reversals of direction.
   * A drawing that reverses is what the eye reads as jitter, and this one is 136px across.
   */
  private placeCup() {
    const img = this.cup()
    const host = this.root.closest('.scrollerwrap') ?? this.root.parentElement ?? this.root
    if (img.parentElement !== host) {
      // Two transcripts in one pane (a session switch that rebuilds the transcript object) must not
      // leave two pictures in the band. The transcript guarantees one cup; this is where that is
      // enforced, since the band belongs to the pane rather than to this transcript's content.
      for (const stale of Array.from(host.children))
        if (stale !== img && stale.classList?.contains('transcript-cup')) stale.remove()
      host.appendChild(img)
    }
    img.classList.toggle('bare', this.root.children.length === 0)
    this.updateCup()
  }

  /**
   * Show the pinned cup only when the reader is at rest at the end of the transcript.
   *
   * The cup is pinned to the pane, but the text under it scrolls. Kept lit while the reader is
   * anywhere else, a 136px drawing sits on top of other people's paragraphs halfway through a
   * conversation - a picture painted over prose is worse than a missing picture. The band the
   * scroller reserves is the end of the content, so at rest the band is empty and the drawing has
   * somewhere to live. It is lit again the moment the view comes back down, which includes every
   * moment the follow is holding the view at the bottom while the model writes.
   */
  private updateCup(readerMoved = false) {
    const img = this.cupImg
    if (!img) return
    const sc = this.hookScroller()
    if (!sc) return
    const away = sc.scrollHeight - sc.clientHeight - sc.scrollTop
    const atEnd = this.root.children.length > 0 && away <= Transcript.CUP_REST_PX
    if (atEnd) {
      if (this.cupHideTimer) {
        clearTimeout(this.cupHideTimer)
        this.cupHideTimer = 0
      }
      this.setCupShown(true)
      return
    }
    if (readerMoved) {
      if (this.cupHideTimer) {
        clearTimeout(this.cupHideTimer)
        this.cupHideTimer = 0
      }
      this.setCupShown(false)
      return
    }
    if (this.cupHideTimer || !this.cupVisible) return
    this.cupHideTimer = window.setTimeout(() => {
      this.cupHideTimer = 0
      this.setCupShown(false)
    }, Transcript.CUP_HIDE_MS)
  }

  private setCupShown(on: boolean) {
    const img = this.cupImg
    if (!img || on === this.cupVisible) return
    this.cupVisible = on
    img.classList.toggle('cup-out', !on)
  }

  /**
   * Re-pick the drawing and its motion. The generating loop runs at the artist's timing with no
   * hold inserted; the resting loop is re-armed every lap with a fresh hold drawn from CUP_IDLE_HOLD
   * (see anipause.ts); reduced motion gets a still, because a media query cannot pause a raster
   * loop. Called whenever the transcript's busy state flips and when the motion setting changes.
   */
  repaintCup() {
    const img = this.cup()
    const busy = !!this.liveWrap
    this.stopCup()
    img.setAttribute('src', this.cupSrc())
    img.classList.toggle('busy', busy)
    if (!busy && !prefersStill()) this.cupHold = new LoopHold(img, this.cupSrc(), CUP_IDLE_HOLD[0], CUP_IDLE_HOLD[1])
  }

  private stopCup() {
    this.cupHold?.stop()
    this.cupHold = null
  }

  private ensureTurn() {
    if (this.turn) return
    this.turn = this.el('turn')
    this.root.appendChild(this.turn)
    // A loop is a property of one turn. A model that repeated itself three turns ago is not looping
    // now, and carrying the history over would mean a map full of the wrong turn.
    this.loop = new LoopWatch()
    this.loopShown = false
    // The cup belongs under the newest thing, so a new turn takes its place.
    this.placeCup()
  }

  /**
   * Start a new visual block. Structural change: allowed to touch layout.
   *
   * A thought does not go through here: it has no body (see `thoughtLine`).
   */
  private block(kind: BlockKind, label: string): HTMLElement {
    this.ensureTurn()
    const wrap = this.el(`blk blk-${kind}`)
    const head = this.el('blk-head', label)
    const body = this.el('blk-body')
    wrap.append(head, body)
    // One name per turn. Said blocks used to repeat "CWOFFEE" down the left edge, once per
    // paragraph. Your own label stays on your bubble (see `.blk-user > .blk-head`), and two of
    // your messages in one turn is still one ask — so the first of them keeps the name and the
    // rest are quiet.
    if (kind === 'user') {
      // Two of your messages in one turn is still one ask, so the first of them carries the
      // name and the rest are quiet. (A message typed while a turn was open lands in that turn
      // and gets its own name later, when the divider moves it into the turn it opens.)
      if (this.turn!.querySelector('.blk-user')) wrap.classList.add('nameless')
    } else {
      this.whoCwoffee()
    }
    // Thinking joins the run accordion; so does any later tool call, even one that
    // happens after the model has said something. What the model *said* goes below the
    // accordion instead of splitting it in two — see `ensureRunGroup`. A note or a warning
    // still lands in the stream: those are the app interrupting, and folding an approval
    // notice into "working on your computer" would hide a fact the user has to see without
    // clicking. (Informational notes no longer come here at all; they go to `onNote`.)
    if (kind === 'reasoning') {
      this.runBodyOf(this.ensureRunGroup()).appendChild(wrap)
    } else {
      // The working pauses while the model talks. It does not end: another tool call joins
      // this same accordion, which is what keeps one turn one block on screen.
      this.closeRunGroup(true)
      this.turn!.appendChild(wrap)
    }
    // Anything that reaches here lands outside the run accordion - your message, what the model
    // said, a note or a failure the app is interrupting for - so it counts as content, and a
    // pinned reader follows it. Thoughts and tool cards never come through here.
    this.scrollIfNeeded('text')
    return body
  }

  /**
   * The one line a thought leaves in the transcript, inside the run accordion.
   *
   * It used to own a body that streamed the model's reasoning into reveal spans: a second body
   * of text growing on every token, under a header, inside an accordion that is closed by
   * default. That is the most expensive thing a long turn can do on a fanless screen - the
   * report was "the preview of the thinking blocks lags the UI too much" - and the words behind
   * it were three clicks away from nobody. So there is no body: no preview, no expander, and
   * the reasoning is never put in the DOM at all. It is still collected, because the store, a
   * branch and the model's cache need the text; only the picture of it is gone.
   *
   * What is left has to carry the liveness the preview used to, so the line counts words while
   * it runs (`thinking · 1.4k words`) and states the duration afterwards (`thought for 9.4s`).
   * A number that climbs is a model that is producing; a number that sits still is the stall
   * the preview used to hide behind its own motion.
   */
  private thoughtLine(): HTMLElement {
    this.ensureTurn()
    const wrap = this.el('blk blk-reasoning')
    const head = this.el('blk-head streaming', 'thinking')
    wrap.append(head)
    // The working is cwoffee's too, so a turn that opens with a thought still gets its name.
    this.whoCwoffee()
    this.runBodyOf(this.ensureRunGroup()).appendChild(wrap)
    const run = this.runGroup
    if (run) this.bumpRun(run, 'think')
    return head
  }

  /**
   * Open the run accordion, or reuse the one already open in this turn.
   *
   * The header is a live claim: it says what this run is doing, and it changes
   * when the run changes from thinking to working. A static "Working on your
   * computer" over a model that has been reasoning for forty seconds would be a
   * label wearing a fact it does not have.
   */
  /**
   * The accordion's shell, built the same way live and stored.
   *
   * Shared on purpose: two builders is how the live turn and the reopened
   * conversation ended up with different affordances, and the reopened one is the
   * one a user compares against their memory of the run.
   */
  private mkRun(): HTMLElement {
    const wrap = this.el('blk blk-run')
    const head = this.el('blk-head')
    head.append(this.el('run-label'), this.el('run-count'))
    const body = this.el('blk-body run-body')
    wrap.append(head, body)
    head.addEventListener('click', () => {
      const open = wrap.classList.toggle('open')
      head.classList.toggle('open', open)
    })
    // Whether the run's entries carry their contents is a setting, not a control here:
    // a `show outputs` button per accordion was a switch each run offered you, when the
    // question ("do I want to read what the steps did?") is answered once, for the whole
    // conversation, and belongs in the app's display settings where a phone and the
    // desktop get the same answer.
    // The two classes travel together on purpose: a run whose body is collapsed cannot show
    // its contents, so "show the working" has to open the accordion as well. Keeping them
    // apart is what made the setting look like it did nothing.
    if (this.verbose) {
      wrap.classList.add('show-out', 'open')
      head.classList.add('open')
    }
    // **Closed by default, live turns included.** The run used to be pinned open while it
    // ran — "the streaming output is the feedback that the app is alive" — and on a local
    // model that pinned-open group is the biggest thing on the screen for a minute, with a
    // dozen tool cards inside it that most people never read. The header is what carries
    // liveness instead: it says `Working on your computer` and counts the steps as they
    // land, and the one thing that must never hide — a question waiting for an answer —
    // forces the group open on its own (see `runForQuestion`).
    wrap.dataset.think = '0'
    wrap.dataset.tool = '0'
    wrap.dataset.ask = '0'
    return wrap
  }

  /**
   * Whether the entries inside a run show what they did.
   *
   * The value is the app's, stored in the conversation database (`ui_prefs`), so the
   * desktop, a browser on another port and a phone all read the same answer. Runs already
   * on screen follow immediately: a toggle that only takes effect on the next turn looks
   * broken on the conversation you are looking at.
   */
  setVerbose(on: boolean) {
    if (this.verbose === on) return
    this.verbose = on
    for (const r of Array.from(this.root.querySelectorAll('.blk-run')) as HTMLElement[]) {
      // Both halves at once: the setting says "show me the working", which means opening the
      // accordion as well as revealing what is inside it.
      r.classList.toggle('show-out', on)
      r.classList.toggle('open', on)
      r.querySelector('.blk-head')?.classList.toggle('open', on)
    }
    // Revealing a run's contents changes the height under the reader, so the view gets
    // the same nudge a fold gets, rather than leaving them somewhere in the middle.
    this.follow('chrome')
  }

  /** Paint one run's header from what it contains. Static so history can call it. */
  static paintRun(wrap: HTMLElement) {
    const label = wrap.querySelector('.run-label')
    const count = wrap.querySelector('.run-count')
    if (!label || !count) return
    const tools = Number(wrap.dataset.tool ?? 0)
    const thinks = Number(wrap.dataset.think ?? 0)
    const running = wrap.classList.contains('running')
    let text: string
    if (running) {
      text = tools > 0 ? 'Working on your computer' : thinks > 0 ? 'Thinking' : 'Working'
    } else if (tools > 0 && thinks > 0) {
      text = 'Thought, then worked on your computer'
    } else if (tools > 0) {
      text = 'Worked on your computer'
    } else {
      const ms = Number(wrap.dataset.thinkMs ?? 0)
      text = ms ? `Thought for ${fmtThink(ms)}` : 'Thought'
    }
    // A run holding a question nobody has answered says so on its face. This is the
    // difference between a folded group that reads as "the app is done, here is what it did"
    // and one that reads as "the app is stopped, and it is waiting on you" — which matters
    // most in exactly the case the folding rule above allows: a reader who folded the group
    // by hand while a card was open.
    const asks = Number(wrap.dataset.ask ?? 0)
    wrap.classList.toggle('awaits', asks > 0)
    // The suffix goes into the label, not after it: assigning the text first and appending
    // to the local variable afterwards is how a waiting run ended up saying nothing about
    // waiting, which is the one thing a folded group has to say.
    label.textContent = asks > 0 ? `${text} · waiting for you` : text
    const steps = tools + thinks
    // While a run is thinking and has not touched a tool yet, the step count says nothing a
    // folded group needs: one step, forever. The live word count is the fact that matters - it
    // tells a reader, without opening the group, that the model is producing rather than
    // stalled, and it is the only sign of life a thought with no preview has.
    const words = Number(wrap.dataset.words ?? 0)
    if (running && tools === 0 && thinks > 0 && words > 0) count.textContent = `${fmtWords(words)} words`
    else count.textContent = steps > 1 ? `${steps} steps` : ''
    // A detected loop stays on the header for the rest of the turn, running or settled: it is the
    // fact a reader needs in order to decide to stop the turn, and afterwards it is the record of
    // why the answer reads the way it does. The quoted phrase is kept short on purpose.
    const loop = wrap.dataset.loop ?? ''
    if (loop) count.textContent = count.textContent ? `${count.textContent} · ${loop}` : loop
    // Nothing else to paint here. An earlier version hid the reveal control on a
    // thinking-only run, which left two kinds of run with different affordances - the
    // display setting applies to every run, so the header only has to say what ran.
  }

  private ensureRunGroup(): HTMLElement {
    this.ensureTurn()
    // The working is cwoffee's too: a turn that opens with a tool call still gets its name.
    this.whoCwoffee()
    const existing = this.runGroup
    if (existing && existing.parentElement === this.turn) {
      // One accordion per turn, reopened rather than replaced. The model putting a
      // paragraph between two tool calls is not three turns pretending to be one: what it
      // said goes under the accordion, and the work carries on inside it. Two accordions
      // stacked above one answer was the mess; this is the same events, one home for the
      // working and one for the saying.
      if (!existing.classList.contains('running')) {
        existing.classList.add('running')
        // A fresh measurement window: the header's "thought for 4.2s" should describe the
        // thinking that is happening now, not the span since the turn first started.
        existing.dataset.opened = String(Math.round(performance.now()))
        delete existing.dataset.thinkMs
        this.paintRunHead(existing)
      }
      return existing
    }
    const wrap = this.mkRun()
    wrap.dataset.opened = String(Math.round(performance.now()))
    wrap.classList.add('running')
    this.turn!.appendChild(wrap)
    this.runGroup = wrap
    this.paintRunHead(wrap)
    return wrap
  }

  private runBodyOf(wrap: HTMLElement): HTMLElement {
    return wrap.querySelector('.run-body') as HTMLElement
  }

  /** Count an entry into the run and update the header, which is what the count is for. */
  private bumpRun(wrap: HTMLElement, which: 'think' | 'tool') {
    wrap.dataset[which] = String(Number(wrap.dataset[which] ?? 0) + 1)
    this.paintRunHead(wrap)
  }

  /** The accordion's header, from what the accordion actually contains. */
  private paintRunHead(wrap: HTMLElement) {
    if (wrap.classList.contains('running') && !wrap.dataset.thinkMs) {
      const thinks = Number(wrap.dataset.think ?? 0)
      const opened = Number(wrap.dataset.opened ?? 0)
      // Remember the duration while it is knowable. A header painted later has no
      // way to recover how long the model actually spent thinking.
      if (thinks > 0 && opened) wrap.dataset.thinkMs = String(Math.round(performance.now() - opened))
    }
    Transcript.paintRun(wrap)
  }

  /**
   * Settle the run accordion. `settle` collapses it: the transcript you scan is a list of
   * what happened, not of how, so a finished run folds to its header.
   *
   * Settling is not the end of the run. The element stays the turn's run, so a tool call
   * that arrives after a paragraph of answer goes back into it (`ensureRunGroup`), which is
   * what keeps a turn to one block of working above one body of saying. The run is dropped
   * when the turn ends.
   *
   * One fold never happens: a run holding a question nobody has answered stays open.
   * Folding it would hide the one card in the turn that is addressed to the reader, and a
   * run whose header they cannot open looks like a hang rather than like a question. The
   * header says so anyway (`waiting for you`) in case the run was folded by hand.
   */
  private closeRunGroup(settle = false) {
    const wrap = this.runGroup
    if (!wrap) return
    wrap.classList.remove('running')
    if (!wrap.dataset.thinkMs) {
      const opened = Number(wrap.dataset.opened ?? 0)
      if (opened && Number(wrap.dataset.think ?? 0) > 0) {
        wrap.dataset.thinkMs = String(Math.round(performance.now() - opened))
      }
    }
    if (settle && !this.verbose && Number(wrap.dataset.ask ?? 0) === 0) {
      // A reader who asked to see the working does not get it folded away at the moment the
      // answer lands. That fold is the disappearance that made a finished turn look like a
      // stack of separate cards.
      wrap.classList.remove('open')
      ;(wrap.querySelector('.blk-head') as HTMLElement)?.classList.remove('open')
    }
    this.paintRunHead(wrap)
    // Folding a run takes height away underneath the reader, which is the one
    // moment a follower ends up short of the bottom without noticing.
    if (settle) this.follow('chrome')
  }

  /**
   * The run group a question card belongs to, opened.
   *
   * A question is the only block in a transcript that is *for* the reader rather than about
   * what the app did, so the folding rule that keeps a long turn scannable cannot be allowed
   * to apply to it. The card used to sit outside the grouped block altogether — which is how
   * "the question and its answer clutter the conversation next to the answer" got reported:
   * a settled question stayed a wide open card between the run and the reply forever.
   * It lives inside the run now, and the run is open for exactly as long as it needs an
   * answer.
   */
  private runForQuestion(): HTMLElement {
    const run = this.ensureRunGroup()
    run.dataset.ask = String(Number(run.dataset.ask ?? 0) + 1)
    if (!run.classList.contains('ask-open')) {
      run.classList.add('ask-open')
      run.classList.add('open')
      run.querySelector('.blk-head')?.classList.add('open')
    }
    this.paintRunHead(run)
    return run
  }

  /** A question stopped needing an answer: let the run that holds it fold again. */
  /** Question cards on screen that still want an answer. */
  private askedCount(): number {
    let n = 0
    for (const card of this.questionCards.values())
      if (!card.classList.contains('answered') && !card.classList.contains('expired')) n++
    return n
  }

  private releaseQuestion(card: HTMLElement) {
    const run = card.closest('.blk-run') as HTMLElement | null
    if (!run) return
    const left = Math.max(0, Number(run.dataset.ask ?? 0) - 1)
    run.dataset.ask = String(left)
    // Whatever closed the card - an answer here, an answer on another device, a stopped turn -
    // the jump button stops claiming someone is waiting.
    this.setAsked(this.askedCount())
    if (left > 0) {
      this.paintRunHead(run)
      return
    }
    // Only fold back what the question itself opened. A group the reader opened by hand
    // stays as they left it.
    if (run.classList.contains('ask-open') && !this.verbose) {
      run.classList.remove('ask-open', 'open')
      run.querySelector('.blk-head')?.classList.remove('open')
    }
    this.paintRunHead(run)
  }

  /** Settle the thought line that just ended: the header becomes the duration. */
  private closeReasoning() {
    this.settleReasoning()
    this.thoughtBuf = null
    this.thoughtWords = 0
  }

  /** Rewrite this turn's thought lines from "thinking" to what they cost. */
  private settleReasoning() {
    for (const head of this.reasoningEls) {
      // The label changes once thinking is over: while it runs "thinking" is the
      // truth, afterwards the useful facts are how long it took and how much came of it.
      const started = Number(head.dataset.thinkStart ?? 0)
      if (started) {
        const words = Number(head.dataset.words ?? 0)
        head.textContent = `${fmtThink(performance.now() - started)}${words ? ` · ${fmtWords(words)} words` : ''}`
        head.classList.remove('streaming')
        head.parentElement?.classList.remove('live')
      }
    }
    this.reasoningEls = []
  }

  /** The live readout on the thought line that is streaming. Cheap on purpose. */
  private paintThought() {
    if (!this.thoughtBuf) return
    this.thoughtBuf.dataset.words = String(this.thoughtWords)
    this.thoughtBuf.textContent = this.thoughtWords >= 40
      ? `thinking · ${fmtWords(this.thoughtWords)} words`
      : 'thinking'
    // The accordion header is what the reader actually sees while the group is folded, so the
    // same number rides there: "Thinking · 1.4k words" is the difference between a model that
    // is working and one that has nothing to say.
    const run = this.runGroup
    if (run?.classList.contains('running')) {
      run.dataset.words = String(this.thoughtWords)
      this.paintRunHead(run)
    }
  }

  /**
   * A loop has been detected in this turn. It is said once: the run header carries it for the rest
   * of the turn, and the app raises one note. Repeating the warning every fifty words would be the
   * same mistake as the loop itself.
   *
   * Nothing is stopped here, on purpose. A wrong stop throws away a real answer mid-flight, and the
   * composer already has a Stop control: the detector's job is to make the loop visible enough that
   * reaching for it is obvious. (The one-line version of "and stop it myself at the third cycle" is
   * `this.host.abort?.()` here, if that is ever wanted.)
   */
  private markLoop(f: LoopFinding) {
    if (this.loopShown) return
    this.loopShown = true
    const phrase = f.phrase.length > 34 ? `${f.phrase.slice(0, 31)}…` : f.phrase
    const shown = { ...f, phrase }
    const run = this.runGroup
    if (run) {
      run.dataset.loop = describeLoop(shown)
      run.classList.add('loops')
      this.paintRunHead(run)
    }
    pageLog(
      `loop detected in turn ${this.turn?.dataset.turn ?? '?'}: ${describeLoop(f)} ` +
        `(${f.words} words after ${f.deltas} deltas)`,
    )
    this.onLoop?.(shown)
  }

  private flush() {
    this.raf = 0
    if (!this.pending.length) return
    const joined = this.pending.join('')
    this.pending = []
    // The loop check runs on the same coalesced chunk the renderer already built, so it costs one
    // call per paint rather than one per token, and it sees the thinking text the reader no longer
    // gets a preview of - which is the whole reason it exists.
    if (this.loop) {
      const hit = this.loop.push(joined)
      if (hit) this.markLoop(hit)
    }
    // A thought has no buffer to paint into - that is the point of it - but its deltas still
    // arrive here, and counting them is what keeps the header honest.
    if (!this.textBuf && this.textKind !== 'reasoning') return
    if (this.textKind === 'answer') {
      this.turnAnswer.push(joined)
      // Answers format as they arrive. The naive version re-parses the whole
      // answer on every frame, which is what makes streaming UIs stutter on a
      // local model; instead only the still-growing tail block is re-parsed and
      // finished blocks are rendered exactly once.
      this.live.push(joined)
      this.renderLive(false)
    } else if (this.textKind === 'reasoning') {
      // A thought is counted, never rendered. The words still have somewhere to go - the event
      // handler keeps them for the store - but putting them in the DOM is what made a thinking
      // turn cost a second growing body of text next to the answer.
      for (const chunk of joined.split(/\s+/)) if (chunk) this.thoughtWords++
      this.paintThought()
    } else if (this.textBuf) {
      this.textBuf.appendChild(document.createTextNode(joined))
    }
    this.scrollIfNeeded(this.textKind === 'reasoning' ? 'work' : 'text')
  }

  /**
   * Render the streaming answer.
   *
   * `force` is used at stream end, where the last block has to be closed even
   * though no blank line will ever arrive for it.
   */
  private renderLive(force: boolean) {
    if (!this.liveEl) return
    if (force) {
      if (this.liveTimer) {
        clearTimeout(this.liveTimer)
        this.liveTimer = 0
      }
      this.liveEl.classList.remove('streaming')
      this.paintAnswer(performance.now(), true)
      // A forced paint is still a paint: if it did not follow, the newest line
      // can sit below the fold until some later event rescues it - which is the
      // lurch coming back through the side door.
      this.follow('text')
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
    this.paintAnswer(now, false)
    // Follow even when nothing committed: a paragraph that has not ended yet
    // still grows a line at a time, and a view that only moves at blank lines
    // leaves the newest line below the fold until the next paragraph lands.
    this.follow('text')
  }

  /**
   * The element the block currently being written lives in.
   *
   * A list item is born as an `<li>` inside a `<ul>`; a paragraph is born as a
   * `<p>`. Deciding that at creation time is the whole trick - the alternative,
   * painting text and re-making the node afterwards, is a rewrite.
   */
  private ensureTail(head = ''): HTMLElement {
    if (this.liveTail) return this.liveTail
    const mark = listMarker(head)
    if (mark && mark.flat && !mark.checkbox) {
      if (!this.liveList) {
        const ul = document.createElement(mark.ordered ? 'ol' : 'ul')
        ul.className = 'live-list'
        this.liveEl!.append(ul)
        this.liveList = ul
      }
      const li = document.createElement('li')
      this.liveList.append(li)
      this.liveTail = li
      this.tailStructured = true
      this.liveTextHost = li
      this.inline.reset()
      this.inlineHost = li
      return li
    }
    this.liveList = null
    this.tailStructured = false
    const t = document.createElement('p')
    t.className = 'live-para'
    this.liveEl!.append(t)
    this.liveTail = t
    this.liveTextHost = t
    // A new paragraph is a new inline scope: no open construct carries over.
    this.inline.reset()
    this.inlineHost = t
    return t
  }

  /**
   * One exception to "built live, leave alone": a link.
   *
   * Inline emphasis and code are opened as they complete, but a link needs both
   * brackets before either half means anything, and research answers put a source
   * in nearly every bullet. So an item with link syntax is parsed once, at its
   * boundary, into the `<li>` that is already on screen: the box stays, only its
   * inline children change, and it happens once per item rather than once per
   * paint.
   */
  private settleItem(el: HTMLElement | null, itemSrc: string) {
    if (!el || !/!?\[[^\]]*\]\([^)]+\)/.test(itemSrc)) return
    const body = itemSrc.replace(/^\s*(?:[-*+]|\d+[.)])\s+/, '').replace(/\n+$/, '')
    const tpl = document.createElement('template')
    tpl.innerHTML = renderMarkdown(body)
    const parsed = Array.from(tpl.content.querySelectorAll('li'))
      .flatMap((li) => Array.from(li.childNodes))
      .concat(
        Array.from(tpl.content.childNodes).filter(
          (n) => (n as HTMLElement).tagName !== 'UL' && (n as HTMLElement).tagName !== 'OL',
        ) as ChildNode[],
      )
    if (parsed.length) el.replaceChildren(...parsed)
  }

  /**
   * Close one list item without closing the list.
   *
   * `listOver` is decided by the source, not by a guess: a blank line after the
   * item ends the list, and the next list in the answer must be its own element
   * rather than more `<li>`s on the end of the old one.
   */
  private endItem(src: string, at: number) {
    this.liveTail = null
    this.inlineHost = null
    this.liveTextHost = null
    this.inline.reset()
    const nextLine = src.slice(at).split('\n', 1)[0] ?? ''
    // The list continues only if what follows is an item, or a marker still
    // arriving. Anything else - prose, a heading, a blank line - ends it, and a
    // later list must be a new element rather than extra `<li>`s on this one.
    if (src.length > at && nextLine.trim() === '') this.liveList = null
    else if (src.length > at && !listMarker(nextLine) && !maybeListMarker(nextLine)) this.liveList = null
  }

  /**
   * Put everything that has arrived on screen, one block at a time.
   *
   * The block being written has its own element, and when its blank line lands
   * the element is closed *in place*. That ordering is the whole point: the
   * paragraph a reader has already looked at keeps the same nodes, the same
   * height and the same position on screen, so committing a block cannot move
   * text that is already being read.
   *
   * Only blocks that the parser would change are rewritten - headings, lists,
   * tables, fences, and paragraphs containing `**` or `_` or links - and each of
   * those is parsed exactly once, ever.
   */
  private paintAnswer(now: number, final: boolean) {
    if (!this.liveEl) return
    const src = this.live.text
    this.lastLiveRender = now
    let guard = 0
    while (guard++ < 200) {
      if (this.livePainted >= src.length) break
      // Ask only about what is still open: where the block being written ends,
      // and whether an unclosed code fence owns the rest of it.
      const rest = src.slice(this.blockStart)
      const rel = firstBlockEnd(rest)
      const head = rest.split('\n', 1)[0]
      const mark = listMarker(head)
      // A paragraph's soft line breaks are joined by the parser anyway, so they
      // are joined here too: painting them as breaks would add a line box that
      // vanishes when the block closes and shifts the page. Lists, tables,
      // quotes and code are meaningful per line, so those keep their newlines.
      const structural =
        fenceOpen(rest) || /^\s*(?:\||```|~~~|[-*+]\s|\d+\.\s|>)/.test(head)
      // A live `<li>` starts after the marker. Painting `- ` into the item and
      // taking it away later is a rewrite of settled text, and the harness caught
      // exactly that: the first chunk landed as "- Sourc" inside the bullet.
      const builtMark = mark && mark.flat && !mark.checkbox ? mark : null
      const contentFrom = this.blockStart + (builtMark ? builtMark.prefix : 0)
      const paintFrom = () => Math.max(this.livePainted, contentFrom)
      // A list item is a finished piece of markdown as soon as the line starting
      // the next one lands. Committing at that boundary, into an `<li>` that was
      // created when the marker arrived, is what makes bullets appear one by one
      // instead of four at a time at the end of the list.
      // Tables: build the skeleton once the delimiter row makes the shape known,
      // then append each completed row. The parser's own rules (splitRow, alignment,
      // padding) do the cells, so a live row and a parsed row are the same thing.
      // A fence that has opened is a block the reader should see as a block, not as backticks.
      // The head arrives with the opening line and the code streams under it; the actions on that
      // head are disabled until the fence closes, because copying half a document or rendering an
      // unclosed <div> is not a thing to offer. Committing still waits for the closing fence.
      if (!this.liveCode) {
        const fs = fenceStart(rest)
        if (fs) {
          const wrap = document.createElement('div')
          wrap.className = 'codeblk streaming'
          wrap.dataset.lang = fs.lang
          wrap.dataset.kind = blockKind(fs.lang)
          wrap.innerHTML = `${codeHeadHtml(fs.lang, true, 0)}\n<pre>${lineGutterHtml(0)}<code${
            fs.lang ? ` class="lang-${fs.lang}"` : ''
          }></code></pre>`
          this.liveEl!.append(wrap)
          const codeEl = wrap.querySelector('pre code') as HTMLElement | null
          if (!codeEl) break
          this.liveCode = {
            wrap,
            code: codeEl,
            lines: 0,
            meta: wrap.querySelector('.code-meta'),
            gutter: wrap.querySelector('pre .ln'),
          }
          this.liveTail = wrap
          this.tailStructured = true
          this.liveTextHost = codeEl
          this.inlineHost = null
          this.inline.reset()
          // The opening fence line is chrome: painting ``` into the body and taking it away later
          // would rewrite settled text. But `blockStart` stays where the block starts, exactly as a
          // list marker's does. Moving it past the fence line made the next `firstBlockEnd` call read
          // the *closing* fence as an opening one - the block then never committed, and the turn ended
          // with an empty, still-streaming block where fifteen lines of HTML had been typed.
          const openLineEnd = Math.min(rest.length, rest.split('\n', 1)[0].length + 1)
          this.livePainted = Math.max(this.livePainted, this.blockStart + openLineEnd)
          continue
        }
      }
      if (!this.liveTable && !fenceOpen(rest)) {
        const headEnd = tableHeadEnd(rest)
        if (headEnd !== null) {
          const lines = rest.slice(0, headEnd).split('\n')
          const tpl = document.createElement('template')
          tpl.innerHTML = tableSkeletonHtml(lines[0], lines[1].trim())
          const table = tpl.content.firstElementChild as HTMLElement | null
          const tbody = table?.querySelector('tbody') ?? null
          if (table && tbody) {
            this.liveEl!.append(table)
            this.liveTable = { delim: lines[1].trim(), tbody }
            this.tailStructured = true
            this.blockStart += headEnd
            this.livePainted = Math.max(this.livePainted, this.blockStart)
            continue
          }
        }
        // A table we cannot prove yet is not a paragraph either. Paint nothing.
        if (tableHeadPending(rest)) break
      }
      if (this.liveTable) {
        const rowEnd = tableRowEnd(rest)
        if (rowEnd !== null) {
          const line = rest.slice(0, rowEnd).replace(/\n+$/, '')
          const tpl = document.createElement('template')
          tpl.innerHTML = tableRowHtml(this.liveTable.delim, line)
          const tr = tpl.content.firstElementChild
          if (tr) this.liveTable.tbody.append(tr)
          this.blockStart += rowEnd
          this.livePainted = Math.max(this.livePainted, this.blockStart)
          continue
        }
        // No complete row yet. Painting the half-row as paragraph text and then
        // replacing it with a `<tr>` is a rewrite of settled text, so wait.
        if (!firstBlockEnd(rest)) break
      }

      const itemEnd = structural && !fenceOpen(rest) ? firstItemEnd(rest) : null
      if (itemEnd !== null) {
        const iend = this.blockStart + itemEnd
        const upto = Math.min(iend, src.length)
        if (upto > paintFrom()) {
          const body = src.slice(paintFrom(), upto).replace(/\n+$/, '')
          if (body) this.appendReveal(this.ensureTail(head), body, now, true, !fenceOpen(rest))
        }
        this.livePainted = Math.max(this.livePainted, upto)
        this.settleItem(this.liveTail, src.slice(this.blockStart, iend))
        this.endItem(src, iend)
        this.blockStart = iend
        continue
      }
      if (!rel && maybeListMarker(head) && !structural) {
        // Half a marker ("-") is not enough to choose a box for this block. Wait a
        // delta rather than painting syntax and rebuilding the box afterwards.
        break
      }
      if (!rel) {
        // Unfinished block: paint what arrived and leave the element open.
        this.appendReveal(
          this.ensureTail(head),
          src.slice(paintFrom(), src.length),
          now,
          !structural,
          !fenceOpen(rest),
        )
        this.livePainted = src.length
        break
      }
      // A closed block. Its text may have arrived in an earlier paint - the
      // blank line usually lands in a later delta than the words - so "closed"
      // is decided by the splitter, never by how far the paint has got.
      const end = this.blockStart + rel.end
      const upto = Math.min(end, src.length)
      if (upto > this.livePainted)
        this.appendReveal(
          this.ensureTail(head),
          src.slice(this.livePainted, upto),
          now,
          !structural,
          !fenceOpen(rest),
        )
      this.livePainted = Math.max(this.livePainted, upto)
      this.closeBlock(src.slice(this.blockStart, end))
      this.blockStart = end
    }
    if (final) this.closeBlock(src.slice(this.blockStart), true)
    if (final) this.liveEl.classList.remove('streaming')
  }

  /**
   * Close the element for a finished block.
   *
   * Most blocks need nothing: a plain paragraph of word spans inside a `<p>` is
   * already the parser's own output, so the element just stops being "live".
   * Rewriting it anyway would re-raster settled text, and re-rastering settled
   * text is what every "blink while streaming" report has traced back to.
   */
  private closeBlock(blockSrc: string, final = false) {
    if (this.liveTable) {
      // A stream that stops mid-table never gives the last row a newline, and a
      // row without its newline is not provably finished - so it is still in the
      // unclaimed part of the source. At the end of the stream there is nothing
      // left it could have been, so the rows left in `blockSrc` are rows. Dropping
      // them is how a live table ended up one row short of what the model typed.
      if (final) {
        for (const line of blockSrc.split('\n')) {
          const t = line.trim()
          if (t === '' || !t.includes('|')) continue
          const tpl = document.createElement('template')
          tpl.innerHTML = tableRowHtml(this.liveTable.delim, t)
          const tr = tpl.content.firstElementChild
          if (tr) this.liveTable.tbody.append(tr)
        }
      }
      this.liveTable = null
    }
    // A fence that closed (or a stream that stopped inside one) is finished in place, not handed
    // to the parser. The block already on screen has the right shape - head, actions, the code as
    // text nodes - and replacing it with freshly parsed markup would re-raster every line of code
    // the reader has already read, which is the one thing this design refuses to do. The old path
    // did exactly that, and worse: the rebuilt block came from a source range the live paint had
    // already consumed, so a fifteen-line HTML answer ended as an empty block whose buttons were
    // still disabled.
    const liveCode = this.liveCode
    this.liveCode = null
    const el = this.liveTail
    const builtLive = this.tailStructured
    this.liveTail = null
    this.inlineHost = null
    this.liveTextHost = null
    this.tailStructured = false
    this.inline.reset()
    if (liveCode && el === liveCode.wrap) {
      this.settleLiveCode(liveCode)
      this.liveList = null
      return
    }
    if (!el) {
      // Nothing was ever painted for this block. That used to mean "empty"; now it
      // can also mean "held back because its first line looked like a table header
      // and the delimiter row had not landed" (see tableHeadPending). At the end of
      // a stream there is nothing left it could have become, so hand the text to the
      // parser rather than silently dropping words the model typed.
      if (final && blockSrc.trim() && this.liveEl) {
        const tpl = document.createElement('template')
        tpl.innerHTML = renderMarkdown(blockSrc)
        this.liveEl.append(...Array.from(tpl.content.childNodes))
      }
      return
    }
    if (builtLive || !el) {
      // An `<li>` built as an `<li>` needs no parser. Running it anyway would
      // replace spans that are already on screen with spans saying the same thing,
      // and the compositor cannot tell that "the same thing" is free.
      this.liveList = null
      return
    }
    if (!blockSrc.trim()) {
      el.remove()
      return
    }
    if (!needsMarkdown(blockSrc)) {
      el.classList.remove('live-para', 'fenced')
      return
    }
    const tpl = document.createElement('template')
    tpl.innerHTML = renderMarkdown(blockSrc)
    const kids = Array.from(tpl.content.childNodes) as HTMLElement[]
    const only = kids.length === 1 ? kids[0] : null
    if (only && only.tagName === 'P') {
      // Same box, new inline markup: keep the element and swap its contents.
      el.classList.remove('live-para', 'fenced')
      el.replaceChildren(...Array.from(only.childNodes))
      return
    }
    // A heading, list, table or fence is a different box; replace the element.
    // `final` keeps the last block's chrome (a code block's copy button) even
    // when the stream stopped mid-block.
    el.replaceWith(...kids)
    if (!final && !kids.length) el.remove()
  }

  /**
   * Flip a live fenced block to its finished shape, without touching the code inside it.
   *
   * What changes when a fence closes is what the buttons are allowed to do, plus two facts the head
   * has to state now that the block is over: how long it is, and whether it is folded.
   */
  private settleLiveCode(st: {
    wrap: HTMLElement
    code: HTMLElement
    lines: number
    meta: HTMLElement | null
    gutter: HTMLElement | null
  }) {
    const { wrap, code } = st
    // The closing fence arrives as text in the same delta that closes the block. The parser would
    // have eaten it, so eat it here: backticks left in the body read as a block that never ended.
    const txt = code.textContent ?? ''
    const tail = /(?:^|\n) {0,3}(?:```|~~~)[ \t]*$/.exec(txt)
    if (tail) code.textContent = txt.slice(0, tail[0].startsWith('\n') ? tail.index : 0)
    const lines = (code.textContent ?? '').split('\n').length
    const kind = blockKind(wrap.dataset.lang ?? '')
    wrap.classList.remove('streaming')
    wrap.dataset.kind = kind
    wrap.dataset.lines = String(lines)
    const folded = foldsWhenClosed(kind, lines)
    if (folded) wrap.classList.add('folded')
    const meta = wrap.querySelector('.code-meta')
    const metaText = lines > 0 ? `${lines} lines` : ''
    if (meta && meta.textContent !== metaText) meta.textContent = metaText
    // The gutter is rewritten once, at the end: while streaming it was appended to line by line, and
    // the closing fence that was eaten above can change the count, so the settled numbers are the
    // ones that are true.
    if (st.gutter) st.gutter.textContent = lineNumbers(lines)
    for (const b of Array.from(wrap.querySelectorAll<HTMLButtonElement>('.code-act'))) {
      b.disabled = false
      b.title = b.title.replace(' - still being written', '')
      if (b.dataset.act === 'fold') {
        b.setAttribute('aria-expanded', folded ? 'false' : 'true')
        b.title = folded ? 'Expand this block' : 'Collapse this block'
      }
    }
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
   * `joinNewlines` renders a paragraph's raw line breaks as spaces, which is
   * what the parser will do with them anyway; doing it here means the paragraph
   * occupies the same number of lines before and after it is committed.
   */
  private appendReveal(
    host: HTMLElement,
    delta: string,
    now: number,
    joinNewlines = false,
    allowInline = true,
  ) {
    // `fenced` belongs to the streaming paragraph only. Toggling a class on a
    // body that is not streaming is an attribute write on a node full of settled
    // text, and every attribute write invalidates that text's style.
    // The live text host is a `<p class="live-para">` or, for a list, the `<li>`
    // being written. Both get inline constructs opened as they complete; a code
    // fence does not, because `**` inside a code block is the text `**`.
    // `fenced` means "this block's newlines are content right now", not "this is a
    // code block" - it goes on any unfinished block whose line breaks must
    // survive. Testing it to decide whether inline markup applies cost a literal
    // `**` on screen, because one paragraph can gain and lose it between paints.
    if (host.classList.contains('live-para')) {
      if (joinNewlines) host.classList.remove('fenced')
      else host.classList.add('fenced')
    }
    // The live text host is a `<p class="live-para">` or, for a list, the `<li>`
    // being written. Both get inline constructs opened as they complete; an open
    // code fence does not, because `**` inside a code block is the text `**`.
    if (this.liveCode && (host === this.liveCode.code || host === this.liveTail)) {
      // A code fence is written, not typeset. No inline markup (`**` inside a fence is the text
      // `**`), and no span per word: a span for every token of a 200-line file is a node cost the
      // reader pays for in scroll and an animation on text that wants to be read, not performed.
      // The block-level paint path hands over the block's own box, not the `<code>` inside it, so
      // the text always goes to `liveCode.code`: writing it to `host` put the code next to the
      // element that renders it, which is how a fifteen-line fence settled as an empty block whose
      // head claimed one line.
      const codeEl = this.liveCode.code
      codeEl.appendChild(document.createTextNode(delta))
      // The head answers "how big is this thing?" while it grows. The count is kept by counting the
      // newlines in the arrival, never the whole buffer: a per-paint scan of a 200KB file is the
      // same mistake the loop detector was built to avoid, and this runs on every paint.
      const st = this.liveCode
      const before = st.lines
      st.lines += (delta.match(/\n/g)?.length ?? 0) + (st.lines === 0 && delta ? 1 : 0)
      const text = st.lines > 0 ? `${st.lines} lines` : ''
      if (st.meta && st.meta.textContent !== text) st.meta.textContent = text
      // The gutter grows with the code rather than being rewritten at the end: the numbers are the
      // reader's place in a document that is still arriving, and the one-node-per-block rule that
      // keeps a finished 200-line file cheap has to hold while it is becoming 200 lines long.
      if (st.gutter && st.lines > before) st.gutter.append(lineGutterDelta(before, st.lines))
      return
    }
    if (host === this.liveTextHost && allowInline) {
      // Inline emphasis in the paragraph being written: open a <strong>/<em>/
      // <code> when the opener is recognised and append the coming words inside
      // it, so `**the copy**` becomes bold the moment it closes instead of at the
      // blank line. Nothing already painted changes: the marked words are born
      // inside the element, the marker characters are never painted, and the text
      // after the construct is appended after it. Reasoning and user bodies are
      // plain text and are left alone.
      for (const tok of this.inline.tokens(delta)) {
        if (tok.kind === 'text') this.appendWords(this.inlineHost ?? host, tok.text, now, joinNewlines)
        else if (tok.kind === 'open') {
          const el = document.createElement(
            tok.mark === '`' ? 'code' : tok.mark === '~~' ? 's' : tok.mark.length === 2 ? 'strong' : 'em',
          )
          ;(this.inlineHost ?? host).append(el)
          this.inlineHost = el
        } else {
          this.inlineHost = this.inlineHost?.parentElement ?? host
        }
      }
      return
    }
    this.appendWords(host, delta, now, joinNewlines)
  }

  private appendWords(host: HTMLElement, delta: string, now: number, joinNewlines = false) {
    const words = revealWords(delta).map((w) => (joinNewlines ? w.replace(/\n/g, ' ') : w))
    if (!revealOn() || !words.length) {
      host.appendChild(document.createTextNode(words.join('')))
      return
    }
    const gap = this.lastPaintAt ? now - this.lastPaintAt : LIVE_RENDER_MS
    this.lastPaintAt = now
    const inst = gap > 0 ? (words.length * 1000) / gap : 0
    this.wordRate = this.wordRate ? this.wordRate * 0.7 + inst * 0.3 : inst
    const dur = revealDuration(this.wordRate)
    const frag = document.createDocumentFragment()
    for (let i = 0; i < words.length; i++) {
      const span = document.createElement('span')
      span.className = 'tin'
      span.textContent = words[i]
      // The duration goes on the span, never on the paragraph. A custom property
      // on the parent re-resolves the style of every settled span inside it, on
      // every paint: an animation that costs the *neighbours* their paint is not
      // an animation, it is flicker with extra steps.
      span.style.animationDuration = `${dur}ms`
      // Stagger inside one arrival so a burst of six words reads as six words
      // arriving, not one block fading in. Capped hard: with a long cap the
      // newest line sits at half-opacity while the model has moved on two lines
      // down, which is the grey "ghost tail" the reveal got blamed for.
      const delay = Math.min(i * 20, 100)
      if (delay) span.style.animationDelay = `${delay}ms`
      frag.append(span)
    }
    host.append(frag)
  }

  /**
   * The scroller, hooked once, and only once it exists.
   *
   * The transcript is constructed before it is in the document, so `closest`
   * answers null at that point; anything that needs the scroll container asks
   * for it lazily.
   */
  private hookScroller(): HTMLElement | null {
    const sc = this.root.closest('.scroller') as HTMLElement | null
    if (!sc || sc === this.scHooked) return sc
    this.scHooked = sc
    // The content's own height is the only reliable signal that something landed which no follow
    // call reported - a tool card, a thought counting words, a table growing a row. The observer
    // does not scroll anything; it keeps the jump button's arithmetic true.
    this.watchHeight()
    sc.addEventListener(
      'scroll',
      () => {
        // Ours, or theirs? Only swallow the event when the position is the one we wrote. A wheel
        // notch that lands in the frame between our write and the flag clearing is a reader saying
        // "stop following", and dropping it both ignores them and keeps them chained to the bottom.
        if (this.programmatic && Math.abs(sc.scrollTop - this.wroteTop) <= 1.5) return
        const away = sc.scrollHeight - sc.scrollTop - sc.clientHeight
        // Reading backwards is a choice; being left behind by the stream is not.
        // 48px is deliberately smaller than a line's worth of growth per paint,
        // so a reader who has been outrun is re-pinned rather than stranded.
        this.pinned = away < 48
        // A reader who moved is a fact about the reader, not a transient of the follow, so the
        // picture is put away on the same gesture rather than after its patience: the jump button
        // appears off the same gesture, and the two must not share the corner of the pane.
        this.updateCup(true)
        pageLog(`follow ${this.pinned ? 'on' : 'off'}: ${Math.round(away)}px from the bottom`)
        // The jump button answers "how far below the fold is the newest line", so it is measured
        // here too: this is the one place a reader's own scroll shows up.
        this.measureFollow()
      },
      { passive: true },
    )
    return sc
  }

  /**
   * Keep the newest line on screen without lurching - for the kind of growth that should.
   *
   * Two rules, both learned from frames rather than from taste. Follow on the frame clock, not
   * the paint clock: the old "jump to the bottom if it is within 160px" rule left the newest line
   * below the fold until a whole paragraph rescued it (112px, 92px, 46px steps), and pinning on
   * every paint only shrank the step to a couple of lines, which still reads as a blink. So the
   * follow runs as its own animation, at most FOLLOW_MAX_STEP px per frame, and only a structural
   * gap larger than FOLLOW_JUMP of the viewport moves at once.
   *
   * The third rule is new, and it exists because the transcript stopped growing where the reader
   * is not looking. With the working folded into one block above the answer, a turn spends most of
   * its life adding height nobody asked to see, and dragging the view down after every tool card
   * is what made the transcript feel like it was running away. So growth is now classified:
   *
   *   `work`   a tool card landing, a build log growing, a thought counting itself - never scrolls
   *   `ask`    a question card waiting for an answer - never scrolls; the jump button says so
   *   `text`   the answer streaming, the deliverable row, a turn's actions - follows when pinned
   *   `chrome` a fold/unfold or the show-the-working setting - follows when pinned, because that
   *            takes height away from under the reader rather than adding it below them
   *   `you`    your own message - the view is placed at the bottom, because you asked for it
   *
   * A reader who scrolled up and stayed there before the answer started is never moved: `pinned`
   * says what they chose, and only `you` and an explicit jump override it.
   */
  private follow(why: Growth = 'chrome') {
    const sc = this.hookScroller()
    if (!sc) return
    if (why === 'you') {
      this.pinned = true
      this.placeAtBottom(sc)
      this.measureFollow(true)
      return
    }
    if (why === 'work' || why === 'ask') {
      // The view holds its ground; the jump button is what tells the reader there is more below.
      this.measureFollow()
      return
    }
    if (!this.pinned) {
      this.stopFollow()
      this.measureFollow()
      return
    }
    this.armFollow()
  }

  /** Put the view at the bottom now, and mark it as the app's own scroll. */
  private placeAtBottom(sc: HTMLElement) {
    this.stopFollow()
    this.scrollTo(sc, sc.scrollHeight - sc.clientHeight)
  }

  /** Write scrollTop and claim the resulting scroll event - but only the one that matches. */
  private scrollTo(sc: HTMLElement, top: number) {
    this.programmatic = true
    sc.scrollTop = top
    this.wroteTop = sc.scrollTop
    requestAnimationFrame(() => {
      this.programmatic = false
    })
  }

  /**
   * Jump to the newest line. This is what the transcript's jump button calls, and it is the only
   * way back to a pinned view other than scrolling there by hand: the button exists because "I
   * read something further up" and "I want the newest line again" are both true of the same person.
   */
  jumpToBottom() {
    const sc = this.hookScroller()
    if (!sc) return
    this.pinned = true
    this.placeAtBottom(sc)
    this.measureFollow(true)
    this.follow()
  }

  // ---- what the reader is looking at, as the app sees it ---------------------
  /** How far the bottom of the content is from the bottom of the view, in px. */
  private away = 0
  /** Question cards waiting for an answer, so the jump button can say "cwoffee is asking". */
  private asked = 0
  private lastEmit = { pinned: true, away: -1e9, ask: false }
  /** The app draws the jump button; this is how it learns when to. */
  onFollowState?: (s: { pinned: boolean; away: number; ask: boolean }) => void

  /**
   * Measure where the reader is and tell the app, but only when the answer moved: a scroll
   * handler, a resize observer and every growth event all ask this question within a frame of each
   * other, and a button that re-renders on each of them is a new source of jank.
   */
  private measureFollow(force = false) {
    const sc = this.hookScroller()
    this.updateCup()
    if (!sc || !this.onFollowState) return
    const away = Math.max(0, Math.round(sc.scrollHeight - sc.clientHeight - sc.scrollTop))
    const ask = this.asked > 0
    const moved = Math.abs(away - this.lastEmit.away) >= 24
    if (!force && !moved && ask === this.lastEmit.ask && this.pinned === this.lastEmit.pinned) return
    this.away = away
    this.lastEmit = { pinned: this.pinned, away, ask }
    this.onFollowState(this.lastEmit)
  }

  /** A question card landed or went away. */
  private setAsked(n: number) {
    this.asked = Math.max(0, n)
    this.measureFollow()
  }

  /** Watch the content's own height: growth that no follow call reports. */
  private watchHeight() {
    if (typeof ResizeObserver === 'undefined') return
    const ro = new ResizeObserver(() => {
      if (this.measureRaf) return
      this.measureRaf = requestAnimationFrame(() => {
        this.measureRaf = 0
        this.measureFollow()
      })
    })
    ro.observe(this.root)
    this.ro = ro
  }

  private ro: ResizeObserver | null = null
  private measureRaf = 0

  /**
   * One step per animation frame, however many events asked for a follow.
   *
   * The first version of the glide moved immediately in `follow()`, so a paint
   * and a tool event and the loop's own frame could each take a step inside the
   * same frame: three 8px steps is a 24px jump, which is the thing being fixed.
   * Events now only arm the loop; the loop is the only thing that scrolls.
   */
  private armFollow() {
    if (this.followRaf) return
    this.followRaf = requestAnimationFrame(() => {
      this.followRaf = 0
      this.stepFollow()
      const sc = this.hookScroller()
      if (sc && this.pinned && sc.scrollHeight - sc.clientHeight - sc.scrollTop > 0.5)
        this.armFollow()
    })
  }

  private stepFollow() {
    const sc = this.hookScroller()
    if (!sc || !this.pinned) return
    const target = sc.scrollHeight - sc.clientHeight
    const gap = target - sc.scrollTop
    if (gap <= 0.5) {
      this.followVel = 0
      // The button and the "how far below am I" number are computed from the same distance the
      // follow loop just closed. Scroll events from our own writing are attributed to us and do not
      // re-measure, so without this the pill kept offering a way down to a reader the loop had
      // already carried to the bottom.
      this.measureFollow(true)
      return
    }
    if (gap > sc.clientHeight * FOLLOW_JUMP) {
      pageLog(`follow: ${Math.round(gap)}px behind is a view change, not a stream; placing the view`)
      this.followVel = 0
      this.scrollTo(sc, target)
      return
    }
    const want = Math.min(gap, Math.max(FOLLOW_MAX_STEP, gap * FOLLOW_CATCHUP))
    const step = Math.min(want, this.followVel + FOLLOW_ACCEL)
    this.followVel = step
    if (step > FOLLOW_MAX_STEP + 1)
      pageLog(`follow catch-up: ${Math.round(gap)}px behind, moving ${Math.round(step)}px this frame`)
    const before = sc.scrollTop
    this.scrollTo(sc, before + step)
    this.measureFollow()
    // The engine rounds scrollTop. If a step bought nothing, finish the trip
    // rather than spin on sub-pixel residue.
    if (sc.scrollTop - before < 0.5) {
      pageLog(`follow: a ${Math.round(step)}px step bought nothing at ${Math.round(gap)}px behind; finishing the trip`)
      sc.scrollTop = target
      this.followVel = 0
    }
  }

  private stopFollow() {
    this.followVel = 0
    if (this.followRaf) {
      cancelAnimationFrame(this.followRaf)
      this.followRaf = 0
    }
  }

  /** Put the reader at the bottom this frame. For switching, never mid-stream. */
  followNow() {
    this.stopFollow()
    this.followVel = 0
    const sc = this.hookScroller()
    if (!sc) return
    this.scrollTo(sc, sc.scrollHeight)
  }

  /** Start a streaming block region: one element per markdown block. */
  private beginLive(el: HTMLElement) {
    this.liveEl = el
    this.live = new LiveMarkdown()
    this.livePainted = 0
    this.liveTail = null
    this.inlineHost = null
    // Every piece of per-block live state belongs here. A field left out survives
    // into the next block, and the next block paints into a box its owner has
    // already forgotten - which is how a table built for one answer can swallow
    // the rows of the next one.
    this.liveTextHost = null
    this.liveList = null
    this.liveTable = null
    this.liveCode = null
    this.tailStructured = false
    this.inline.reset()
    this.blockStart = 0
    this.lastLiveRender = 0
    this.lastPaintAt = 0
    el.classList.add('streaming')
  }
  private push(s: string, kind: BlockKind) {
    // A thought owns no buffer - that is the point of it - so "is this still the same block?"
    // cannot be asked of `textBuf` for reasoning: asked that way, every delta opened a new thought
    // line, which is one header line per word.
    if (this.textKind !== kind || (!this.textBuf && kind !== 'reasoning')) {
      // Drain first. `flush` joins the whole queue and routes it by whatever
      // `textKind` is *at paint time*, so a queue that survives the switch paints
      // the tail of a thinking block inside the answer - and glues that prose to
      // the table that follows it into one paragraph that can never parse. That
      // is the bug behind "the table is in the store but never became a table":
      // the reasoning deltas and the first answer delta landed in the same frame.
      this.flushNow()
      // Leaving an answer block: finish its markdown before anything else is
      // appended, so a tool call in the middle of an answer cannot truncate the
      // formatting of the part already on screen.
      if (this.textKind === 'answer') this.finalizeAnswer()
      if (this.textKind === 'reasoning') this.closeReasoning()
      if (kind === 'reasoning') {
        // A line, not a body: see `thoughtLine`. `textBuf` stays null so nothing downstream
        // can paint reasoning text into the page by accident.
        this.textBuf = null
        this.thoughtBuf = this.thoughtLine()
        this.thoughtBuf.dataset.thinkStart = String(performance.now())
        this.thoughtWords = 0
        this.reasoningEls.push(this.thoughtBuf)
      } else {
        const label = kind === 'user' ? 'you' : 'cwoffee'
        this.textBuf = this.block(kind, label)
        if (kind === 'answer') {
          this.answerEls.push(this.textBuf)
          this.beginLive(this.textBuf)
        }
      }
      this.textKind = kind
      this.setLive(this.thoughtBuf ?? this.textBuf)
    }
    this.pending.push(s)
    if (!this.raf) this.raf = requestAnimationFrame(() => this.flush())
  }

  /**
   * Report a change in the transcript, with what kind of change it was: see `follow`.
   */
  private scrollIfNeeded(why: Growth = 'chrome') {
    this.follow(why)
  }

  /** Where a reader deliberately is: false once they scroll up, true at the bottom. */
  get following(): boolean {
    return this.pinned
  }

  // ---- event application -------------------------------------------------

  apply(ev: EngineEvent) {
    switch (ev.kind) {
      case 'turnStart': {
        this.flushNow()
        // Your own words belong to *this* turn, but they are already on screen: the composer
        // paints them the moment you press send, while `this.turn` still points at the turn
        // that just finished. Starting a new turn and putting the divider at its top therefore
        // slid the divider *under* your message, which reads as your message belonging to the
        // answer above it. So the trailing user blocks — and their name marker — come along
        // into the turn they belong to, and the divider lands above them.
        const prev = this.turn
        const trailing: HTMLElement[] = []
        if (prev) {
          for (
            let n = prev.lastElementChild;
            n && n.classList.contains('blk-user');
            n = n.previousElementSibling as HTMLElement | null
          ) {
            trailing.unshift(n as HTMLElement)
          }
        }
        this.turn = null
        this.textBuf = null
        this.textKind = null
        // A turn's files are that turn's. Left over, the previous turn's chips get
        // re-parented into the new one by the next `addFiles` - which is how a
        // "clean up those downloads" turn ended up wearing the files it deleted.
        this.turnFiles = []
        this.fileEls = []
        this.runGroup = null
        this.collected = { answer: '', reasoning: '' }
        this.answerEls = []
        const head = this.el('turn-head', `turn ${ev.d.turn_id} · ${ev.d.model}`)
        this.ensureTurn()
        this.turn!.appendChild(head)
        for (const n of trailing) {
          // The one that opens this turn carries your name again: it was quiet in the turn it
          // happened to be typed into, and here it is the line the turn starts with.
          n.classList.remove('nameless')
          this.turn!.appendChild(n)
        }
        trailing.slice(1).forEach((n) => n.classList.add('nameless'))
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
        // Out of the transcript and into the chrome. An event that arrives through apply()
        // carries no conversation id, so it is not remembered for replay either — it would
        // follow the reader into unrelated sessions. Notes are the app talking about itself;
        // they are worth seeing once, and they were making the record of a conversation hard
        // to read.
        this.flushNow()
        this.showNote(ev.d.message)
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
        // The working surface folds once the answer has landed: what you scan a
        // finished turn for is what happened, and the header carries that.
        this.closeRunGroup(true)
        // Actions after the answer, not before the files: the answer is what you
        // read, the files are what it produced, and copying is a thing you do to
        // a finished turn rather than one still arriving.
        this.appendTurnActions()
        // And then the turn stops paying for its own animation. See the method.
        this.collapseRevealSpans()
        break
      case 'usage':
      case 'toolCallArgs':
      case 'toolExecStart':
        break
    }
  }

  /**
   * Drop the reveal scaffolding from a finished turn.
   *
   * One `<span class="tin">` per word is what buys the word-by-word arrival, and it
   * is also what makes a long session hard to scroll. Measured in the app's own
   * webview (WebKitGTK, `scripts/perf-rig.mjs --mode live`): a hundred streamed
   * turns leave **200,905 nodes** in the transcript, of which **192,694 are `.tin`
   * spans** — spans that finished animating turns ago and now hold nothing. Load the
   * same conversation from history and the same text arrives as 2,931 nodes, which is
   * exactly the report: reopening a chat feels fine, *living* in one does not.
   *
   * The span earns its keep while the word arrives and costs after that. The reveal
   * is a CSS animation with `backwards` fill and no post-animation class, so a
   * finished span has no visual state left to hold: replacing it with the text node it
   * wraps changes nothing you can see, and `wordAt`/`charAt`/`pointToCaret`/the quote
   * and copy paths already read plain text nodes. A span that wraps markup — a link,
   * a code span, a bold run, a hard break — keeps its element, because there the
   * element is the meaning and not the animation.
   *
   * The last few words are left alone, and swept on a timer: replacing a span whose
   * animation has not finished would pop the word into view, and the tail of a turn is
   * precisely where a pop would be noticed.
   */
  collapseRevealSpans(): void {
    // The negative control for the measurement above: with this off, the spans stay.
    // `scripts/perf-rig.mjs --collapse off` sets it, so the claim "collapsing is what
    // fixed it" can be falsified in the same run that makes it.
    if (!this.collapseSettled || !this.turn) return
    const turn = this.turn
    const spans = Array.from(turn.querySelectorAll<HTMLElement>('.tin'))
    if (!spans.length) return
    const tail = Math.min(24, spans.length)
    let collapsed = 0
    for (const s of spans.slice(0, spans.length - tail)) collapsed += collapseSpan(s)
    pageLog(`live: collapsed ${collapsed} reveal span(s) in the finished turn`)
    if (!tail) return
    setTimeout(() => {
      if (!turn.isConnected) return
      let rest = 0
      for (const s of Array.from(turn.querySelectorAll<HTMLElement>('.tin'))) rest += collapseSpan(s)
      if (rest) pageLog(`live: collapsed the last ${rest} reveal span(s) after their animation`)
    }, 1500)
  }

  /** Flush any buffered deltas. Call before reading textContent or ending a turn. */
  flushNow() {
    if (this.raf) cancelAnimationFrame(this.raf)
    this.raf = 0
    if (this.liveTimer) {
      clearTimeout(this.liveTimer)
      this.liveTimer = 0
    }
    // "Now" means now: drop the throttle window so the next flush paints rather
    // than scheduling itself again.
    this.lastLiveRender = 0
    this.flush()
  }

  /**
   * Close out the streaming answer.
   *
   * Every block the model finished has already been committed as it landed;
   * this closes the one block that never got its blank line, so the live result
   * matches what `renderMarkdown` would produce for the stored text - without a
   * second pass over blocks that are already correct.
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
    this.liveTail = null
    this.inlineHost = null
    this.inline.reset()
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
    // Your own words are the one thing that always moves the view: a message typed while the
    // reader was looking further up belongs at the bottom, and they wrote it.
    this.follow('you')
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

  /**
   * Paint the part of a running turn that happened before this client arrived.
   *
   * A page pointed at the gateway and reloaded mid-answer is handed the stored rows, which
   * stop at the last closed message. Everything streamed since then is in the host's memory,
   * not on disk yet, so the reloaded page would otherwise paint the reply starting at the
   * first delta that arrives after the reload — the top of the message missing, fixing itself
   * only when the turn ends and its row is written. The host hands over that open half and
   * this puts it on screen, fed through the same buffer the live deltas use so the answer
   * continues *inside the same block* rather than starting a second one.
   *
   * It defers to whatever is already streaming: if deltas arrived before the host's answer
   * did, painting the earlier half now would put it below the later half, so it is dropped
   * instead of mis-ordered.
   */
  seedOpen(text: string, reasoning: string) {
    if (!text && !reasoning) return
    if (this.textBuf) return
    if (reasoning) {
      this.push(reasoning, 'reasoning')
      this.flushNow()
    }
    if (text) {
      this.push(text, 'answer')
      this.flushNow()
    }
  }

  /** Surface a shell-level failure in the transcript, not just the log. */
  /** A quiet informational line: migration notices, backend defaults. */
  /**
   * Show an announcement about the app itself: a resumed history, a compaction,
   * a backend default. Not part of any answer, and deliberately not part of any
   * turn's replay buffer either - a note about *this app* is not evidence about
   * the conversation, and gating it by turn ownership made it vanish (the switch
   * that emits it also triggers the repaint that would have shown it).
   *
   * `session` says which conversation the note is about; `null` means the app as
   * a whole. The last few are kept so a repaint of that conversation brings them
   * back rather than leaving the reader with a transcript that lost its only
   * explanation.
   */
  showNote(msg: string, session?: number, paintHere = true) {
    this.notes.push({ text: msg, session: session ?? null })
    if (this.notes.length > NOTE_MEMORY) this.notes.shift()
    // A note about conversation 18 does not belong on screen while the reader is
    // looking at 3. It is kept, and replayNotes brings it back the moment 18 is on
    // screen - which, for the note a session switch emits, is one repaint later.
    if (paintHere && this.onNote) this.onNote(msg, session ?? null)
  }

  /**
   * Re-render the notes that belong to a conversation after its transcript is
   * rebuilt. Only notes stamped with a session come back: an unstamped note has no
   * conversation to be loyal to, and replaying one into every transcript turns a
   * one-line notice into a stalker.
   *
   * One notice, not all of them: these are now transient toasts, and replaying six
   * stacked notes about a conversation the reader is arriving at is a wall of chrome.
   * The newest one is the one that explains what just happened.
   */
  replayNotes(session: number) {
    const mine = this.notes.filter((n) => n.session === session)
    if (mine.length && this.onNote) {
      const last = mine[mine.length - 1]
      const more = mine.length - 1
      this.onNote(more > 0 ? `${last.text} (+${more} earlier note${more > 1 ? 's' : ''})` : last.text, session)
    }
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
      // The card no longer needs an answer, so the grouped run holding it may fold again the
      // way any other finished run does. Without this the question keeps its group open for
      // the rest of the conversation, which is the clutter the report was about.
      this.releaseQuestion(card)
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
    // The card belongs to the run that asked it - inside the grouped block, not beside
    // it - and that group is open for as long as the card needs an answer. A question
    // that can hide behind a folded header is a hang wearing a transcript.
    this.runForQuestion().querySelector('.run-body')!.appendChild(card)
    this.questionCards.set(q.id, card)
    // A question waiting for an answer is the one thing in the transcript that must not hide, and
    // it does not need the view dragged down to avoid hiding: the jump button says "cwoffee is
    // asking" and the group holding the card is forced open (see `runForQuestion`). Dragging the
    // view down instead would interrupt a reader who was reading something else.
    this.setAsked(this.askedCount())
    this.scrollIfNeeded('ask')
  }

  /**
   * Several questions asked in one `ask_user` call, as one card.
   *
   * The engine sends them together and waits for the set as a unit, so the card
   * collects an answer per question and sends them in one submit. Two properties the
   * single-question card does not need:
   *
   *   * navigating between questions keeps each pane's selection and typed text -
   *     re-rendering from scratch loses the words someone typed two questions ago;
   *   * after submit the card collapses to one line and expands to a read-only list
   *     of question/answer pairs. It never becomes interactive again: the tool call
   *     that asked has already returned, and a live option row on a settled card is
   *     a button that does nothing, or worse, answers a call that is gone.
   */
  showQuestionSet(
    qs: QuestionRequested[],
    onAnswer: (id: number, r: { text: string; options: number[] }) => void,
  ) {
    // The helpers below are plain functions, and `this` inside a plain function is
    // whatever called it. Bind the Transcript once, explicitly.
    const self = this
    this.flushNow()
    this.ensureTurn()
    const ordered = qs
      .slice()
      .sort((a, b) => (a.set?.index ?? 0) - (b.set?.index ?? 0))
    const n = ordered.length
    const card = this.el('blk blk-ask is-set')
    const head = this.el('blk-head')
    const label = this.el('blk-label', n > 1 ? `asking ${n}` : 'asking')
    head.append(label)
    const body = this.el('ask-body')
    card.append(head, body)

    const nav = this.el('ask-nav')
    const count = this.el('ask-count')
    const dots = this.el('ask-dots')
    const back = this.btn('ask-back', 'Back')
    const next = this.btn('ask-next', 'Next')
    nav.append(count, dots, back, next)
    body.append(nav)

    // One pane per question, built once and kept. Only one is in the document at a
    // time; the others wait detached, with their state intact.
    type Pane = {
      q: QuestionRequested
      root: HTMLElement
      sel: Set<number>
      input: HTMLInputElement | null
      rows: HTMLElement[]
      answered: () => boolean
      summary: () => string
    }
    const panes: Pane[] = ordered.map((q) => {
      const root = this.el('ask-pane')
      root.append(this.el('ask-q', q.question))
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
        row.append(this.el('ask-mark', q.multi ? '\u2610' : '\u25CB'))
        const main = this.el('ask-opt-main')
        const title = this.el('ask-opt-title')
        title.append(this.el('ask-lbl', o.label))
        if (q.recommended === i) {
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
      root.append(opts)
      let input: HTMLInputElement | null = null
      if (q.allow_custom) {
        const row = this.el('ask-custom')
        input = document.createElement('input')
        input.className = 'ask-input'
        input.placeholder = q.multi
          ? 'or type your own answer (it is sent with any boxes you tick)'
          : 'or type your own answer instead'
        row.append(input)
        root.append(row)
      }
      const paint = () => {
        rows.forEach((row, i) => {
          const on = sel.has(i)
          row.classList.toggle('is-sel', on)
          row.setAttribute('aria-checked', on ? 'true' : 'false')
          const mark = row.querySelector('.ask-mark')
          if (mark) mark.textContent = q.multi ? (sel.has(i) ? '\u2611' : '\u2610') : sel.has(i) ? '\u25CF' : '\u25CB'
        })
        paintNav()
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
            advance()
          }
        })
      }
      const picked = () => [...sel].sort((a, b) => a - b)
      const typed = () => (input?.value ?? '').trim()
      return {
        q,
        root,
        sel,
        input,
        rows,
        answered: () => sel.size > 0 || typed().length > 0,
        summary: () => {
          const labels = picked().map((i) => q.options[i]?.label ?? `#${i + 1}`)
          const t = typed()
          if (!labels.length && !t) return 'no answer'
          if (!labels.length) return `typed: ${t}`
          return t && t !== labels.join(', ') ? `${labels.join(' + ')} \u00b7 ${t}` : labels.join(' + ')
        },
      }
    })

    const stage = this.el('ask-stage')
    body.append(stage)
    const actions = this.el('ask-actions')
    const send = this.btn('ask-submit', 'Send answers')
    send.disabled = true
    const decline = this.btn('ask-decline', 'Decline')
    actions.append(send, decline)
    body.append(actions)
    const hint = this.el('ask-hint')
    body.append(hint)

    let cur = 0
    const answeredCount = () => panes.filter((p) => p.answered()).length

    function paintNav() {
      count.textContent = n > 1 ? `Question ${cur + 1} of ${n}` : 'The model is asking'
      dots.replaceChildren(
        ...panes.map((p, i) => {
          const d = document.createElement('button')
          d.type = 'button'
          d.className = `ask-dot${i === cur ? ' is-cur' : ''}${p.answered() ? ' is-done' : ''}`
          d.setAttribute('aria-label', `Question ${i + 1} of ${n}${p.answered() ? ' (answered)' : ''}`)
          d.addEventListener('click', () => show(i))
          return d
        }),
      )
      const done = answeredCount()
      send.disabled = done === 0
      send.textContent = done === n ? 'Send answers' : `Send ${done} of ${n} answer${n === 1 ? '' : 's'}`
      back.disabled = cur === 0
      next.disabled = cur === n - 1
      hint.textContent =
        done > 0 && done < n
          ? `${n - done} question${n - done === 1 ? '' : 's'} would be sent as no answer`
          : ''
    }

    function show(i: number) {
      if (card.classList.contains('answered')) return
      cur = Math.max(0, Math.min(n - 1, i))
      stage.replaceChildren(panes[cur].root)
      paintNav()
    }

    function advance() {
      const gap = panes.findIndex((p, i) => i !== cur && !p.answered())
      if (gap >= 0) show(gap)
      else if (answeredCount() === n) submit()
      else show(Math.min(n - 1, cur + 1))
    }

    function submit() {
      if (card.classList.contains('answered')) return
      const answers = panes.map((p) => {
        const picked = [...p.sel].sort((a, b) => a - b)
        const typed = (p.input?.value ?? '').trim()
        const labels = picked.map((i) => p.q.options[i].label)
        // Picks go in `options`, the person's own words in `text`: the engine reports
        // the second as "User also said", which is what a constraint added beside a
        // choice is.
        const text = picked.length
          ? typed && typed !== labels.join(', ')
            ? typed
            : labels.join(', ')
          : typed
        return { id: p.q.id, text, options: picked }
      })
      card.classList.add('answered')
      // The interactive part goes away entirely. Disabling it leaves a card that
      // looks answerable forever, which is the complaint about the old one.
      nav.remove()
      stage.remove()
      actions.remove()
      hint.remove()
      const got = answers.filter((a) => a.text).length
      body.append(
        self.el('ask-answered', got ? `answered ${got} of ${n}` : 'declined - the model was told you answered nothing'),
      )
      const qa = self.el('ask-qa')
      panes.forEach((p, i) => {
        const row = self.el('ask-qa-item')
        row.append(self.el('ask-qa-q', n > 1 ? `${i + 1}. ${p.q.question}` : p.q.question))
        row.append(self.el('ask-qa-a', p.summary()))
        qa.append(row)
      })
      body.append(qa)
      head.classList.add('is-toggle')
      head.setAttribute('role', 'button')
      head.setAttribute('aria-expanded', 'false')
      head.tabIndex = 0
      head.addEventListener('click', () => {
        const open = card.classList.toggle('is-open')
        head.setAttribute('aria-expanded', open ? 'true' : 'false')
      })
      head.addEventListener('keydown', (e) => {
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault()
          head.click()
        }
      })
      label.textContent = got ? `answered ${got} of ${n}` : 'declined'
      answers.forEach((a) => onAnswer(a.id, { text: a.text, options: a.options }))
      // The card no longer needs an answer, so the group holding it may fold like any
      // other finished run. `releaseQuestion` only closes what the question opened.
      self.releaseQuestion(card)
      self.scrollIfNeeded('text')
    }

    back.addEventListener('click', () => show(cur - 1))
    next.addEventListener('click', () => show(cur + 1))
    send.addEventListener('click', submit)
    // Declining answers every question with nothing, which is what the engine reports
    // as "answered none" rather than as a choice it may invent.
    decline.addEventListener('click', () => {
      panes.forEach((p) => {
        p.sel.clear()
        if (p.input) p.input.value = ''
      })
      submit()
    })

    // The question joins the run that asked it, and that run stays open until it is
    // answered: see `runForQuestion`.
    this.runForQuestion().querySelector('.run-body')!.appendChild(card)
    for (const p of panes) this.questionCards.set(p.q.id, card)
    show(0)
    this.setAsked(this.askedCount())
    this.scrollIfNeeded('ask')
  }

  /** Close a question card answered elsewhere - another tab, another device. */
  settleQuestion(id: number) {
    const card = this.questionCards.get(id)
    if (!card || card.classList.contains('answered')) return
    card.classList.add('answered', 'by-other')
    card.append(this.el('ask-answered', 'answered elsewhere'))
    this.releaseQuestion(card)
  }

  /**
   * Close a question that closed *without* an answer.
   *
   * This is no longer about a clock. By default a question does not expire at all — the
   * turn is parked, not failing — so what reaches here is the tool stopping its wait: the
   * person pressed Stop, or the reply channel died with a window that went away. (A clock
   * is still possible if `question_timeout_secs` is written down on purpose.) Saying
   * "nobody answered" is the difference between a dead button and an explained one.
   *
   * `why` says which of those happened, because the three sentences are not
   * interchangeable: "the model was told it got none" about a turn the reader stopped
   * themselves describes an event that never took place, and it is the one line in the
   * transcript that a stopped question leaves behind. No `why` (an older stream, or a card
   * closed by a reply the host had already let go of) gets the wording that is true for
   * all three.
   */
  expireQuestion(id: number, why?: 'turnStopped' | 'timedOut' | 'windowGone') {
    const card = this.questionCards.get(id)
    if (!card || card.classList.contains('answered') || card.classList.contains('expired')) return
    const said =
      why === 'turnStopped'
        ? 'no answer — the turn was stopped; the tool did not run'
        : why === 'timedOut'
          ? 'no answer — the clock ran out; the model was told it got none and to decide for itself'
          : why === 'windowGone'
            ? 'no answer — the window holding this card closed'
            : 'no answer — the card closed; the model was told it got none'
    card.classList.add('expired')
    card.append(this.el('ask-expired', said))
    card
      .querySelectorAll('.ask-opt, .ask-input, .ask-submit, .ask-decline')
      .forEach((n) => {
        n.classList.add('is-done')
        if (n instanceof HTMLInputElement || n instanceof HTMLButtonElement) n.disabled = true
      })
    card.querySelectorAll('.ask-opt').forEach((n) => (n as HTMLElement).setAttribute('aria-disabled', 'true'))
    this.releaseQuestion(card)
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
    for (const el of this.fileEls) el.remove()
    this.fileEls = this.fileRow(this.turnFiles)
    for (const el of this.fileEls) this.turn!.appendChild(el)
    this.closeRunGroup(true)
    this.scrollIfNeeded('text')
  }

  private fileRow(files: OutputFile[]): HTMLElement[] {
    // One entry per path, newest write winning. The host records every write, so a model that
    // rebuilt the same workbook three times left three near-identical units in the strip — the
    // same file, and only the newest of them is on disk. Three chips each pretend to be a
    // different deliverable; how many writes it took is better told in the tooltip.
    const byPath = new Map<string, OutputFile & { writes: number }>()
    for (const f of files) byPath.set(f.path, { ...f, writes: (byPath.get(f.path)?.writes ?? 0) + 1 })
    const all = [...byPath.values()]
    const row = this.el('filestrip')
    const line = this.el('attachline')
    // Three kinds of file, three shapes. An attachment is the model saying "this one is
    // for you" — it chose to write into `.cws/assistant_attachments/`, so it is the only
    // file the host will copy somewhere the person picks, and it never folds. An
    // artifact is a generated file to open from the chat. A file the model changed while
    // implementing a feature wants a count and a list: a real feature in a real
    // repository is dozens of files, and a strip of 38 chips used to bury the one
    // artifact in the turn. Below the threshold, chips are the right shape for changes
    // too — two edited files are worth seeing.
    const deliverables = all.filter((f) => f.kind === 'artifact')
    const attachments = all.filter((f) => f.kind === 'attachment')
    // A file the model fetched for itself landed under .cws/ on purpose, so the
    // transcript does not present it as something the person asked for. It is
    // still one click away — hidden is not the same as gone, and a file the model
    // read should be checkable by the person who was told about it.
    const scratch = all.filter((f) => (f as { kind?: string }).kind === 'scratch')
    const changes = all.filter(
      (f) => f.kind !== 'artifact' && f.kind !== 'attachment' && (f as { kind?: string }).kind !== 'scratch',
    )
    // Attachments on their own line, against the right edge of the column. They are the files the
    // model wrote *to hand over*, and they are read differently from the working strip: the strip
    // says what the turn touched (left, under the prose, in the order it touched them), the line
    // says what it made for you. Sharing one left-aligned row made a deliverable look like one more
    // file it edited - which is how a person ends up hunting for the thing they came back to fetch.
    for (const f of attachments) line.append(this.attachmentUnit(f))
    for (const f of deliverables) row.append(this.fileChip(f))
    if (scratch.length) row.append(this.scratchGroup(scratch))
    if (changes.length <= ROLL_FILES_AT) {
      for (const f of changes) row.append(this.fileChip(f))
    } else if (changes.length) {
      row.append(this.changeRoll(changes))
    }
    // An empty line is not a line: a turn that produced no attachment must not reserve a band of
    // blank column above its working chips.
    return [line, row].filter((el) => el.childElementCount > 0)
  }

  /**
   * An attachment and the control that hands it over.
   *
   * The button is separate from the chip because the chip opens the file *where it
   * already sits* — in the assistant's folder — while a person asking for a file
   * usually means "put it somewhere I chose". The transcript cannot pick a destination
   * and does not try: it asks the shell, which on the desktop opens a save dialog and
   * on a browser tab downloads.
   */
  private attachmentUnit(f: OutputFile & { writes?: number }): HTMLElement {
    const unit = this.el('attachunit')
    // A file the model meant you to have — and say where it is, rather than drawing a box
    // around it. The folder starts with a dot, so no file manager, screenshot or listing shows
    // it: a solid border promises a container that is not there, and a second box inside a box
    // is what turned a filename into a 200px block. A dashed hairline is the outline of a box
    // that is deliberately not quite there.
    if (f.writes && f.writes > 1)
      unit.title = `${f.name} — written ${f.writes} times this turn; the ${fmtSize(f.bytes)} copy is what is on disk`
    unit.append(this.fileChip(f))
    // The save control is an icon, like every other action on a block: the row is one line tall and
    // the filename is the thing that has to stay readable. What it does, and what just happened,
    // live in `title` and `aria-label` - a name in those costs no pixels and is still announced.
    const idle = `Save ${f.name} (${fmtSize(f.bytes)}) wherever you choose`
    const btn = this.btn('attach-save', '')
    btn.innerHTML = actIcon('download', 13)
    btn.title = idle
    btn.setAttribute('aria-label', `Save ${f.name} to a place you choose`)
    btn.addEventListener('click', async (e) => {
      e.stopPropagation()
      if (!this.onDeliver) {
        if (this.onToast) this.onToast('this connection cannot save files', true)
        return
      }
      btn.disabled = true
      btn.title = `Saving ${f.name}…`
      try {
        const msg = await this.onDeliver(f.path)
        if (msg && this.onToast) this.onToast(msg)
        // The glyph changes to a tick and stays that way for a moment: the file picker has already
        // taken the reader's attention, so a word they may not have seen is not enough proof.
        btn.classList.add('done')
        btn.innerHTML = actIcon('check', 13)
        btn.title = `Saved ${f.name}`
      } catch (err) {
        // The tick has to go back at once. A download glyph that turned into a tick and then lied
        // is worse than no answer at all, and the failure path is exactly where a reader looks for
        // proof. The reason goes to the toast, not the console.
        btn.classList.remove('done')
        btn.innerHTML = actIcon('download', 13)
        btn.title = idle
        if (this.onToast) this.onToast(`could not save ${f.name}: ${err}`, true)
      } finally {
        btn.disabled = false
        setTimeout(() => {
          btn.classList.remove('done')
          btn.innerHTML = actIcon('download', 13)
          btn.title = idle
        }, 2500)
      }
    })
    unit.append(btn)
    return unit
  }

  /** The reference files of a turn, folded into one line: `2 reference files`. */
  private scratchGroup(files: OutputFile[]): HTMLElement {
    const wrap = this.el('refgroup')
    const head = this.btn('refhead', `${files.length} reference file${files.length === 1 ? '' : 's'}`)
    head.title = 'Files the model fetched for its own use. They live under .cws/ in the workspace, which is git-ignored.'
    const list = this.el('reflist')
    for (const f of files) {
      const chip = this.fileChip(f)
      chip.classList.add('ref')
      chip.title = `${f.path} — reference copy under .cws/`
      list.append(chip)
    }
    list.hidden = true
    head.addEventListener('click', () => {
      list.hidden = !list.hidden
      head.classList.toggle('open', !list.hidden)
    })
    wrap.append(head, list)
    return wrap
  }

  private fileChip(f: OutputFile): HTMLElement {
    const chip = document.createElement('button')
    chip.className = 'filechip'
    chip.type = 'button'
    chip.dataset.path = f.path
    // The size belongs in the tooltip for an attachment: copying a 400 kB page to your
    // own folder and copying a 400 MB archive are different acts, and the chip looks
    // identical either way.
    chip.title =
      f.kind === 'attachment'
        ? `${f.name} — ${fmtSize(f.bytes)} in .cws/assistant_attachments/. Click to open it here, Save to copy it wherever you choose.`
        : `Open ${f.path}`
    const ico = this.el('file-ico')
    ico.append(fileIcon(f.mime))
    chip.append(ico)
    const meta = this.el('file-meta')
    meta.append(this.el('file-name', f.name))
    // An attachment is one line: the mime type of a file the model already told you about is
    // not information, and the size is what you weigh before saving a copy. The rest of the
    // detail is in the tooltip.
    if (f.kind === 'attachment') {
      chip.classList.add('one-line')
      meta.append(this.el('file-sub', fmtSize(f.bytes)))
    } else {
      meta.append(this.el('file-sub', `${shortMime(f.mime)} · ${fmtSize(f.bytes)}`))
    }
    chip.append(meta)
    return chip
  }

  /**
   * One line for "the model changed a lot of files", expandable to the list.
   *
   * Every path stays clickable: collapsing a strip into a count must not cost
   * access to any file in it. The folders are named when there are few, because
   * "38 files · src, tests" answers the question the reader actually has.
   */
  private changeRoll(files: OutputFile[]): HTMLElement {
    const wrap = this.el('fileroll')
    const head = document.createElement('button')
    head.type = 'button'
    head.className = 'fileroll-head'
    head.setAttribute('aria-expanded', 'false')
    const bytes = files.reduce((sum, f) => sum + (f.bytes || 0), 0)
    const dirs = [...new Set(files.map((f) => (f.path.includes('/') ? f.path.split('/')[0] : '.')))]
    head.append(
      this.el('fileroll-count', `${files.length} files written`),
      this.el('fileroll-size', `${fmtSize(bytes)}${dirs.length <= 3 ? ` · ${dirs.join(', ')}` : ''}`),
    )
    const list = this.el('fileroll-list')
    list.hidden = true
    for (const f of files) list.append(this.fileChip(f))
    head.onclick = () => {
      const show = list.hidden
      list.hidden = !show
      head.classList.toggle('open', show)
      head.setAttribute('aria-expanded', String(show))
    }
    wrap.append(head, list)
    return wrap
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

    const share = btn(
      'share',
      'Share as Markdown',
      'Save this conversation as a Markdown file, where you choose',
      iconDoc(),
    )
    share.onclick = () => {
      if (!this.onShare) {
        if (this.onToast) this.onToast('sharing is not available here', true)
        return
      }
      share.disabled = true
      void this.onShare(session)
        // A toast is the receipt, not a chip: the conversation does not gain a
        // file because someone exported it.
        .then((msg) => {
          if (this.onToast) this.onToast(msg)
        })
        .catch((e) => {
          if (this.onToast) this.onToast(`share failed: ${e}`, true)
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
    // The row lands after the last paint, so a reader who was being followed
    // ends up 30-odd pixels short of the bottom exactly when the turn closes.
    this.follow('text')
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
      // Inside a run accordion the entry does not expand on its own; the run's
      // `show outputs` reveals every entry's contents at once.
      if (card.closest('.blk-run') && !card.closest('.show-out')) return
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
    // The call joins the run accordion. It is open while it runs because the
    // output streaming past is the feedback that the app is alive; it folds back
    // to one line when the call finishes.
    card.classList.add('live')
    const run = this.ensureRunGroup()
    this.bumpRun(run, 'tool')
    this.runBodyOf(run).appendChild(card)
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
    this.scrollIfNeeded('work')
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
    this.scrollIfNeeded('work')
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
    t.card.classList.remove('live')
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
    // One cup for the page, under everything the store produced, resting: a loaded session is a
    // finished one, so this is the loop with the random holds rather than a still.
    this.placeCup()
    this.repaintCup()
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
    // The run for the turn being rendered. One per turn here too, and it survives the
    // paragraphs in between: `blk` settles it, the next reasoning row reopens it, and only a
    // user row — the line where one turn stops and the next starts — throws it away. The
    // stored rows stay exactly as they are and in the order they are in; only their
    // arrangement on screen changes, because rewriting history mid-conversation is how a
    // prompt cache stops matching and every turn starts costing full price again.
    let run: HTMLElement | null = null
    // One name per turn here too. A stored transcript has no `.turn` element to bound the
    // marker, so it sticks to the scroller until the next turn replaces it — the same
    // affordance seen from the other end: whoever's words fill the screen is named at the top
    // of it. `asked` counts as cwoffee speaking: a question the agent asked is its turn.
    let whoSaid = false
    const whoCwoffee = () => {
      if (whoSaid) return
      whoSaid = true
      const wrap = this.el('turn-who turn-who-cwoffee')
      wrap.append(this.el('who-name', 'cwoffee'))
      frag.appendChild(wrap)
    }
    // The last thing this page's turns actually said. One actions row per turn, the
    // same affordance a live turn puts under its final answer - emitted per assistant
    // row, it stamped `Copy | Share as markdown | Branch` under every intermediate
    // message and the transcript read as a stack of separate turns.
    let lastAnswer: { body: HTMLElement; text: string } | null = null
    const flushAnswerActs = () => {
      if (!lastAnswer) return
      const acts = this.turnActionsRow(lastAnswer.text, this.sessionId)
      acts.classList.add('from-history')
      // Appended at the end of what this turn has produced so far, which is where a
      // live turn puts them: under the turn, after any run that followed its last
      // sentence. Inserting them under the answer itself would slide them above a run
      // that came after it and read as belonging to the next turn.
      frag.appendChild(acts)
      lastAnswer = null
    }
    const blk = (kind: string, label: string) => {
      endRun()
      if (kind !== 'user') whoCwoffee()
      const wrap = this.el(`blk blk-${kind}`)
      const head = this.el('blk-head', label)
      const body = this.el('blk-body')
      wrap.append(head, body)
      frag.appendChild(wrap)
      lastWrap = wrap
      return body
    }
    // Stored turns are grouped the same way live turns are. If the two were
    // grouped differently, a conversation would look tidier while it ran than
    // after you left and came back, and the second look is the one people trust.
    // The display setting, captured so the nested settle-run can honour it: a stored transcript
    // has to open its runs exactly like the live one, or the same conversation looks different
    // depending on whether you are watching it or coming back to it.
    const verboseRuns = this.verbose
    const runBody = () => {
      if (!run) {
        whoCwoffee()
        run = this.mkRun()
        frag.appendChild(run)
        lastWrap = run
      }
      return run!.querySelector('.run-body') as HTMLElement
    }
    function endRun() {
      if (!run) return
      run.classList.remove('running')
      if (!verboseRuns) {
        run.classList.remove('open')
        run.querySelector('.blk-head')?.classList.remove('open')
      }
      // The header carries the run's summary, so `Thought for 4.2s` needs the
      // thinking duration the row stored, not an estimate made now.
      if (!run.dataset.thinkMs) {
        const first = run.querySelector('.blk-reasoning') as HTMLElement | null
        const ms = Number(first?.dataset.thinkMs ?? 0)
        if (ms) run.dataset.thinkMs = String(ms)
      }
      Transcript.paintRun(run)
      // Not `run = null`: this turn's accordion is still the place its next working block
      // belongs, and a paragraph of answer in between is not a reason to build a second one.
    }

    for (const m of msgs) {
      if (m.role === 'user') {
        // A user row is where one turn stops and the next starts, so it is where the
        // previous turn's actions row gets placed — and where this turn's accordion is
        // dropped, so the next turn gets its own rather than inheriting this one's steps.
        flushAnswerActs()
        endRun()
        run = null
        whoSaid = false
        const body = blk('user', 'you')
        body.textContent = m.content
        this.appendAttachmentTags(body, m.attachments ?? [])
      } else if (m.role === 'assistant') {
        if (m.reasoning) {
          // A line, not a body: the stored reasoning is not rendered at all. It is still counted,
          // so a session reopened hours later says the same thing about the thought that was said
          // while it was live - and counting it costs a scan of text that used to cost a thousand
          // spans in the page.
          const wrap = this.el('blk blk-reasoning')
          const words = countWords(m.reasoning)
          const ms = Number(m.reasoning_ms ?? 0)
          const head = this.el(
            'blk-head',
            ms
              ? `${fmtThink(ms)}${words ? ` · ${fmtWords(words)} words` : ''}`
              : words
                ? `${fmtWords(words)} words of thinking`
                : 'thinking',
          )
          wrap.append(head)
          if (ms) wrap.dataset.thinkMs = String(ms)
          if (words) wrap.dataset.words = String(words)
          const rb = runBody()
          rb.appendChild(wrap)
          run!.dataset.think = String(Number(run!.dataset.think ?? 0) + 1)
        }
        // An assistant row with no text in it is a turn's bookkeeping: the row that
        // exists because the model spent its step on a tool call. Painting it as an
        // answer produced a `CWOFFEE` label, an empty body, and a `Copy | Share as
        // markdown | Branch` line under nothing - and that bare strip between two run
        // accordions is what made a settled turn look *un*grouped, right after the live
        // turn had looked grouped. A turn's actions belong to what it said, so nothing
        // said, nothing offered.
        const saidSomething = (m.content ?? '').trim().length > 0
        if (saidSomething) {
          const body = blk('answer', 'cwoffee')
          body.innerHTML = renderMarkdown(m.content)
          // The source, kept on the element: right-click "Copy" and any future
          // export from the DOM should hand back what the model wrote, not the
          // rendering of it.
          body.dataset.md = m.content
          lastAnswer = { body, text: m.content }
        }
        if (m.files?.length) {
          // Into the fragment, directly under the answer that produced them. This
          // used to call ensureTurn() and append to the live turn, which landed the
          // chips at the *top* of a reloaded transcript, above every message: a
          // session reopened after writing a file looked like it had no attachment,
          // which is exactly the "the file was not shown at turn end" report.
          for (const el of this.fileRow(m.files)) frag.insertBefore(el, lastWrap ? lastWrap.nextSibling : null)
        }
        for (const call of parseCalls(m.tool_calls_json)) {
          if (call.name === 'ask_user') {
            // No tool card, live or in history: a card headed `ASK_USER` names the
            // mechanism and hides the only interesting part, which is the question
            // the agent actually asked. The answer (the tool result) lands in the
            // same block, so the exchange reads as one exchange.
            // Not a note: this exchange is part of what was said, and notes are the app
            // talking about itself, which now lives outside the transcript. A question the
            // agent asked and the answer given are the conversation's own record, so they
            // stay in the stream, as their own block kind.
            const body = blk('asked', 'asked you')
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
          const rb = runBody()
          rb.appendChild(card)
          run!.dataset.tool = String(Number(run!.dataset.tool ?? 0) + 1)
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
    // The page may end in the middle of a run. Its header still has to say what
    // the run was: an unpainted header reads as an empty accordion.
    endRun()
    flushAnswerActs()
    return frag
  }
}

/** Minimum interval between live markdown passes. */
/**
 * How many changed files a turn can list as individual chips before they become
 * one line. Four is the point where "files the model worked through" stops being
 * a list you read and starts being a wall you skip - and where the deliverable in
 * the same turn (an artifact, always shown as its own chip) disappears.
 */
const ROLL_FILES_AT = 4

/**
 * How many app announcements stay attachable to a transcript. Enough that the
 * explanation for a slow turn is still there after a few switches back and forth;
 * small enough that a long-lived window cannot accumulate a wall of old notes.
 */
const NOTE_MEMORY = 6

const LIVE_RENDER_MS = 120

/**
 * How fast the view may chase the bottom while following, in CSS px per frame.
 *
 * A paint happens every 120ms and can add two or three lines, so pinning to the
 * bottom *at paint granularity* still moves the whole transcript by 23-33px in a
 * single frame - measured in the user's own screen recording as 46px and 66px
 * steps at 4.9s and 5.7s. Everything already on screen teleports, and the eye
 * reads that as a blink. 6px/frame glides at ~360px/s, which is three times the
 * fastest this endpoint streams text and slow enough to read as motion rather
 * than a jump.
 */
const FOLLOW_MAX_STEP = 6

/**
 * When the view has fallen further behind, catch up by a fraction of the gap
 * each frame rather than in one jump. A hard "snap if more than 150px" rule was
 * the first attempt and it is its own lurch: 151px of accumulated text - or one
 * big tool card, or one slow frame on a busy machine - moved the whole page in a
 * single frame. Closing a seventh of the gap per frame covers 200px in about 16
 * frames and never has a discontinuity to hide.
 */
const FOLLOW_CATCHUP = 0.14

/**
 * How fast the follow may speed up, in px/frame per frame.
 *
 * A fixed fraction of the gap makes the first frame of a catch-up jump straight
 * to full speed: when a table or a tool card lands below the fold the view moves
 * 29px instantly, which is the same single-frame displacement the lurch had. The
 * velocity now ramps, so the same catch-up reads as a glide that gathers speed.
 */
const FOLLOW_ACCEL = 6

/**
 * A gap more than a screen and a half wide is not text arriving; it is a view
 * changing under the reader - a session opened, an older page loaded. Gliding
 * through 3000px of transcript is not smooth motion, it is a mistake, so the
 * view is placed rather than walked.
 */
const FOLLOW_JUMP = 1.5

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
  sheet_read: {
    run: 'Reading a spreadsheet',
    done: 'Read a spreadsheet',
    bad: 'Could not read that spreadsheet',
  },
  sheet_query: {
    run: 'Asking the spreadsheet a question',
    done: 'Answered from the spreadsheet',
    bad: 'Could not query that spreadsheet',
  },
  write_xlsx: { run: 'Writing a spreadsheet', done: 'Wrote a spreadsheet', bad: 'Could not write the spreadsheet' },
  download_file: { run: 'Downloading a file', done: 'Downloaded a file', bad: 'Could not download that file' },
  web_fetch: { run: 'Opening a page', done: 'Opened a page', bad: 'Could not open that page' },
  web_search: { run: 'Searching the web', done: 'Searched the web', bad: 'The search did not parse' },
  exec: { run: 'Running a command', done: 'Ran a command', bad: 'The command did not parse' },
  // The app's own history, read out loud. "Recalling memory" while it works, "Recalled"
  // when it has said what it found — and the failure wording names the reason, because a
  // tool host that was never given the store answers differently from a search that came
  // up empty, and the transcript should not flatten those two into one grey row.
  recall: { run: 'Recalling memory', done: 'Recalled', bad: 'Could not reach the conversation store' },
  // The model asking what else this app can do. Worth its own words because in lean mode it
  // is the door to every capability the transcript has not shown yet, and the row is where a
  // person sees that a tool appeared partway through a turn.
  explore: { run: 'Looking through the tool set', done: 'Found the tools', bad: 'Could not read the tool set' },
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
