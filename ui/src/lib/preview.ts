/**
 * Previews for html and svg code blocks.
 *
 * The content being previewed is untrusted. It arrives from a model that quotes
 * web pages it fetched, so this module treats every block as hostile input.
 *
 * Two rules:
 *
 *  - HTML goes into an iframe with `sandbox` (no `allow-same-origin`) plus a
 *    strict CSP injected ahead of the document. Scripts run, because a preview
 *    that cannot run the snippet is not much of a preview - but the frame has an
 *    opaque origin, no access to the app, and `default-src 'none'` so it can
 *    make no network request at all, including to the IPC endpoint.
 *
 *    The measured basis: a sandboxed frame in this webview exposes neither
 *    `window.__TAURI__` nor `window.__TAURI_INTERNALS__` (the self-test probes
 *    for both and logs the result). The CSP closes the remaining route, which is
 *    a raw `fetch()` to the custom IPC protocol rather than the JS bridge.
 *  - SVG goes into an <img>. An SVG document loaded as an image runs no scripts
 *    by definition, so inline handlers and <script> inside the markup are inert
 *    without us having to parse or sanitise the SVG at all.
 */

// The markdown renderer, for a ```md block that wants to be read as prose rather than as source.
// One direction only: markdown.ts knows nothing about previews, so this import cannot cycle.
import { renderMarkdown } from './markdown'

// Must land inside the first 1024 bytes of the document, hence prepended.
// default-src 'none' kills fetch/XHR/WebSocket/form posts; the rest re-enables
// only what a static-ish preview needs to look right.
const CSP_META =
  '<meta http-equiv="Content-Security-Policy" content="default-src \'none\'; ' +
  "script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data: blob:; " +
  'font-src data:; media-src blob:">'

let panelEl: HTMLElement | null = null
let lastUrl: string | null = null

function close() {
  if (lastUrl) {
    URL.revokeObjectURL(lastUrl)
    lastUrl = null
  }
  panelEl?.remove()
  panelEl = null
  document.removeEventListener('keydown', onKey)
}

function onKey(e: KeyboardEvent) {
  if (e.key === 'Escape') {
    e.stopPropagation()
    close()
  }
}

function shell(title: string): HTMLElement {
  const root = document.createElement('div')
  root.className = 'pv'
  root.setAttribute('role', 'dialog')
  root.setAttribute('aria-label', `${title} preview`)

  const head = document.createElement('div')
  head.className = 'pv-head'
  const label = document.createElement('span')
  label.className = 'pv-title'
  label.textContent = title
  const note = document.createElement('span')
  note.className = 'pv-note'
  const x = document.createElement('button')
  x.className = 'pv-x'
  x.type = 'button'
  x.textContent = 'close'
  x.onclick = close
  head.append(label, note, x)

  const body = document.createElement('div')
  body.className = 'pv-body'

  root.append(head, body)
  root.onclick = (e) => {
    if (e.target === root) close()
  }
  document.addEventListener('keydown', onKey)
  return root
}

/** Open a preview panel. Returns a short reason when the kind is not previewable. */
export function openPreview(kind: 'html' | 'svg', source: string): void {
  close()
  const root = shell(kind === 'html' ? 'HTML preview' : 'SVG preview')

  if (kind === 'svg') {
    // An <img> is the whole security story: no scripts, no external resources
    // from the app origin, no DOM to reach.
    const blob = new Blob([source], { type: 'image/svg+xml' })
    lastUrl = URL.createObjectURL(blob)
    const img = document.createElement('img')
    img.className = 'pv-svg'
    img.alt = 'SVG preview'
    img.src = lastUrl
    img.onerror = () => {
      img.replaceChildren('The SVG did not parse.')
      img.className = 'pv-err'
    }
    ;(root.querySelector('.pv-note') as HTMLElement).textContent = 'rendered as an image; scripts disabled'
    ;(root.querySelector('.pv-body') as HTMLElement).append(img)
  } else {
    const f = document.createElement('iframe')
    f.className = 'pv-frame'
    // allow-scripts so the snippet actually works; no allow-same-origin, so the
    // frame stays in an opaque origin with nothing of ours to read.
    f.setAttribute('sandbox', 'allow-scripts')
    f.srcdoc = CSP_META + source
    ;(root.querySelector('.pv-note') as HTMLElement).textContent =
      'sandboxed; scripts run but network access is blocked'
    ;(root.querySelector('.pv-body') as HTMLElement).append(f)
  }

  panelEl = root
  document.body.append(root)
}

/**
 * The rendered form of a fenced block, inside the block itself.
 *
 * The panel answers the same question at panel size; this answers it where the reader already is,
 * which is what makes a folded block worth folding: the model wrote two hundred lines of markup,
 * and what the reader wants is the page those lines make, not the lines.
 *
 * The safety story is the panel's, unchanged. HTML goes into a sandboxed frame with no
 * `allow-same-origin` and the same strict CSP, so it can reach neither the app nor the network;
 * SVG is loaded as an image, which runs no scripts by definition; markdown goes through the
 * renderer that escapes everything it was not asked to interpret.
 *
 * Returns true when the rendered view is now showing, false when it was just turned off, so the
 * button that called this can say which state it left the block in.
 */
export function inlineRender(blk: HTMLElement, source: string): boolean {
  const kind = blk.dataset.kind ?? 'code'
  const existing = blk.querySelector('.pv-inline')
  if (existing) {
    existing.remove()
    blk.classList.remove('has-inline')
    return false
  }
  if (kind === 'svg') {
    inlineSvg(blk, source)
    return true
  }
  if (kind === 'html') {
    const f = document.createElement('iframe')
    f.className = 'pv-inline pv-inline-frame'
    f.setAttribute('sandbox', 'allow-scripts')
    f.srcdoc = CSP_META + source
    f.title = 'Rendered HTML'
    blk.append(f)
    blk.classList.add('has-inline')
    return true
  }
  if (kind === 'md') {
    const d = document.createElement('div')
    d.className = 'pv-inline md-render'
    // renderMarkdown escapes text and only produces the tags it knows about; this is the same
    // renderer the answer text itself goes through.
    d.innerHTML = renderMarkdown(source)
    blk.append(d)
    blk.classList.add('has-inline')
    return true
  }
  return false
}

/**
 * Inline SVG under the code block. Same <img> trick, so the inline view is no
 * more dangerous than the panel view.
 */
export function inlineSvg(host: HTMLElement, source: string): void {
  const existing = host.querySelector('.pv-inline')
  if (existing) {
    existing.remove()
    host.classList.remove('has-inline')
    return
  }
  const blob = new Blob([source], { type: 'image/svg+xml' })
  const url = URL.createObjectURL(blob)
  const img = document.createElement('img')
  img.className = 'pv-inline'
  img.alt = 'SVG preview'
  img.src = url
  img.onerror = () => {
    URL.revokeObjectURL(url)
    img.replaceChildren('The SVG did not parse.')
    img.className = 'pv-inline pv-err'
  }
  host.append(img)
  host.classList.add('has-inline')
}
