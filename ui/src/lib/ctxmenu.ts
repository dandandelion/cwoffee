/**
 * The right-click menu.
 *
 * WebKitGTK's menu is the browser's: Back, Forward, Reload, and a selection
 * "Copy". None of those mean anything in a frameless app window - and the one
 * that does mean something, Copy, is buried under the ones that do not. So the
 * menu is ours: whatever is under the pointer decides what it offers.
 *
 * One deliberate exception: inside a textarea or text input the native menu is
 * left alone. The webview's own Cut/Copy/Paste entries there are wired to the
 * clipboard in ways a page cannot reach - `document.execCommand('paste')` is
 * blocked for script - so suppressing the native menu in a field would take away
 * paste rather than improve the menu. Ctrl+V still works, but a menu that offers
 * an item which silently does nothing is worse than no menu.
 */

export type CtxItem = {
  label: string
  hint?: string
  /** Returns a short string to toast, or nothing to close quietly. */
  run: () => void | Promise<void>
  disabled?: boolean
}

export type CtxHooks = {
  /** Toast feedback for actions with no visible result of their own. */
  toast: (msg: string, bad?: boolean) => void
  /** Put text into the composer without sending it. */
  quote: (text: string) => void
  openFile: (path: string) => void
}

let el: HTMLDivElement | null = null

function close() {
  if (!el) return
  el.remove()
  el = null
}

async function copy(text: string, toast: CtxHooks['toast']) {
  try {
    if (navigator.clipboard?.writeText) {
      await navigator.clipboard.writeText(text)
      toast('copied')
      return
    }
  } catch {
    /* fall through */
  }
  try {
    const ta = document.createElement('textarea')
    ta.value = text
    ta.style.position = 'fixed'
    ta.style.top = '-1000px'
    document.body.appendChild(ta)
    ta.select()
    const ok = document.execCommand('copy')
    ta.remove()
    toast(ok ? 'copied' : 'the webview refused the clipboard', !ok)
  } catch {
    toast('the webview refused the clipboard', true)
  }
}

/**
 * The text a block holds, preferring what the model actually wrote over what it
 * looks like: `**bold**` copied as "bold" cannot be pasted back into anything.
 * History rows carry the source in data-md because they are rendered from it.
 */
function markdownOf(blk: Element): string {
  const body = blk.querySelector('.blk-body')
  const raw = body?.getAttribute('data-md')
  if (raw) return raw
  return blkText(blk)
}

function blkText(blk: Element): string {
  const b = blk.querySelector('.blk-body') ?? blk
  // <br> and block boundaries are not implied by textContent; without this a
  // copied multi-paragraph answer arrives as one line.
  return (b.textContent ?? '').replace(/\u00a0/g, ' ').trim()
}

function itemsFor(target: HTMLElement, h: CtxHooks): CtxItem[] {
  const out: CtxItem[] = []
  const sel = window.getSelection()
  const selected = sel && !sel.isCollapsed ? (sel.toString() ?? '').trim() : ''

  const chip = target.closest('.filechip') as HTMLElement | null
  if (chip?.dataset.path) {
    const path = chip.dataset.path
    out.push({ label: 'Open file', hint: chip.dataset.name ?? path, run: () => h.openFile(path) })
    out.push({ label: 'Copy path', run: () => void copy(path, h.toast) })
  }

  const codeBtn = target.closest('.code-act') as HTMLElement | null
  const codeblk = target.closest('.codeblk') as HTMLElement | null
  if (codeblk) {
    const code = codeblk.querySelector('pre code')?.textContent ?? ''
    out.push({ label: 'Copy code', run: () => void copy(code, h.toast) })
    if (codeBtn && codeBtn.textContent?.toLowerCase().includes('preview')) {
      out.push({ label: 'Preview', run: () => codeBtn.click() })
    }
  }

  const blk = target.closest('.blk') as HTMLElement | null
  if (blk && !blk.closest('.blk-tool')) {
    const who = blk.classList.contains('blk-user') ? 'your message' : 'the answer'
    const md = markdownOf(blk)
    if (md) {
      out.push({ label: `Copy ${who}`, run: () => void copy(md, h.toast) })
      if (!blk.classList.contains('blk-user')) {
        out.push({ label: 'Quote in composer', run: () => h.quote(md) })
      }
    }
  }

  const card = target.closest('.blk-tool') as HTMLElement | null
  if (card) {
    const tool = card.dataset.tool ?? ''
    const text = (card.textContent ?? '').trim()
    if (tool) out.push({ label: 'Copy tool call', hint: tool, run: () => void copy(text, h.toast) })
  }

  if (selected) out.push({ label: 'Copy selection', run: () => void copy(selected, h.toast) })

  const sess = target.closest('.sessrow') as HTMLElement | null
  if (sess) {
    const title = sess.querySelector('.sess-title')?.textContent ?? ''
    if (title) out.push({ label: 'Copy session title', run: () => void copy(title, h.toast) })
  }

  if (!out.length) return []
  // Two copies of the same thing is noise: if a selection was copied, that wins.
  if (selected && out.length > 2) {
    const i = out.findIndex((x) => x.label.startsWith('Copy ') && x.label !== 'Copy selection')
    if (i >= 0 && out[i + 1]?.label === 'Copy selection') out.splice(i, 1)
  }
  return out
}

function show(x: number, y: number, items: CtxItem[], h: CtxHooks) {
  close()
  const menu = document.createElement('div')
  menu.className = 'ctxmenu'
  menu.setAttribute('role', 'menu')
  for (const it of items) {
    const b = document.createElement('button')
    b.type = 'button'
    b.className = 'ctx-item'
    b.setAttribute('role', 'menuitem')
    b.textContent = it.label
    if (it.hint) {
      const hint = document.createElement('span')
      hint.className = 'ctx-hint'
      hint.textContent = it.hint
      b.append(hint)
    }
    b.disabled = !!it.disabled
    b.onclick = async (e) => {
      e.stopPropagation()
      close()
      await it.run()
    }
    menu.append(b)
  }
  document.body.appendChild(menu)
  el = menu
  // Measure then place: a menu clipped at the right or bottom edge of a phone
  // window is a menu with missing items.
  const r = menu.getBoundingClientRect()
  const pad = 8
  const left = Math.max(pad, Math.min(x, window.innerWidth - r.width - pad))
  const top = Math.max(pad, Math.min(y, window.innerHeight - r.height - pad))
  menu.style.left = `${left}px`
  menu.style.top = `${top}px`
  menu.querySelector<HTMLButtonElement>('.ctx-item:not(:disabled)')?.focus()
}

/**
 * Install on the document. Returns a teardown; the app lives for the session, so
 * it exists mostly so a test can undo it.
 */
export function installContextMenu(h: CtxHooks): () => void {
  const onContext = (e: MouseEvent) => {
    const t = e.target as HTMLElement | null
    if (!t) return
    // Fields keep the native menu; see the note at the top of this file.
    if (t.closest('textarea, input, [contenteditable]')) return
    const items = itemsFor(t, h)
    if (!items.length) {
      // Nothing meaningful to offer is still a reason to suppress Back/Reload:
      // navigating the webview away from the app is the worst thing a stray
      // right-click can do.
      e.preventDefault()
      return
    }
    e.preventDefault()
    show(e.clientX, e.clientY, items, h)
  }
  const dismiss = (e: Event) => {
    if (el && !el.contains(e.target as Node)) close()
  }
  const key = (e: KeyboardEvent) => {
    if (e.key === 'Escape' && el) {
      e.stopPropagation()
      close()
    }
  }
  document.addEventListener('contextmenu', onContext, true)
  document.addEventListener('pointerdown', dismiss, true)
  document.addEventListener('keydown', key, true)
  window.addEventListener('blur', close)
  // Deliberately not on scroll: the transcript scrolls itself to follow the stream,
  // and a menu that closes because the *model* moved the page is a menu the user
  // never got to use. It is position:fixed, so it stays where it was opened.
  return () => {
    document.removeEventListener('contextmenu', onContext, true)
    document.removeEventListener('pointerdown', dismiss, true)
    document.removeEventListener('keydown', key, true)
    window.removeEventListener('blur', close)
    close()
  }
}
