/*
 * The bridge cwoffee installs in every page it opens, before any of the page's own code runs.
 *
 * It has two jobs, and both have to hold up against a page that was not written for us:
 *
 * 1. It watches. console.*, uncaught exceptions, unhandled rejections, failed fetch/XHR, failed
 *    <img>/<script>/<link> loads — each one kept as a structured record with a timestamp, the kind
 *    of thing it is, and which navigation it belongs to. The page's own console.log is *not* a
 *    failure, and the record says so rather than leaving the model to guess.
 * 2. It acts. `__cw.run(job)` performs one step — click, fill, press, scroll, read — and puts the
 *    answer in `__cwR[id]`. The app polls for that answer instead of trusting an eval callback,
 *    because wry documents that a callback registered against a webview that is still loading gets
 *    dropped: a lost answer would be a hang, and a hang is not an error message.
 *
 *    A step that cannot answer in the tick it was asked — `wait`, `wait_for`, an `eval` that handed
 *    back a promise — leaves its answer in `__cwR[id]` when it is ready, inside the budget the job
 *    states. The app is the one that stops waiting, and the page is the one that knows how long it
 *    actually waited, so the number the model reads is measured in the page rather than promised.
 *
 * The target language is the one `browser/session.mjs` uses — label, placeholder, role+name, text,
 * then selector, first unique match wins, an ambiguity names how many matched — so the model is
 * told the same thing about a page whichever browser it is looking at.
 *
 * This script has no privileges. It is inside an untrusted page, on the page's side of the only
 * door: it can read and change its own document, and nothing else.
 */
(() => {
  'use strict'
  if (window.__cw && window.__cw.version === 5) return

  const cw = (window.__cw = {})
  cw.version = 5
  // A fresh id per document. The app compares it: a different boot means a new page, whoever
  // caused it — the model's `navigate`, a link the person clicked, or `location.href` in the page.
  cw.boot = Math.random().toString(36).slice(2)

  // The page's own arrival notice, in the same tick this script starts. It is not a diagnostic —
  // there is nothing wrong with a page that has loaded — it is the page saying what it is and where
  // it is, before any of its own code has had a chance to move the address. A panel that instead
  // asks the question a moment later is asking the page's own redirect where it *was*.
  try {
    window.__cwR = window.__cwR || {}
    window.__cwR['boot:' + cw.boot] = JSON.stringify({
      ok: true,
      url: location.href,
      title: document.title,
      ready: document.readyState,
      boot: cw.boot,
    })
  } catch (e) {
    /* nothing we can do about our own notice; the page carries on */
  }

  window.__cwR = window.__cwR || {}
  cw.done = function (id) {
    delete window.__cwR[id]
  }

  /* ---------------------------------------------------------------- watching */

  const CAP = { di: 240, net: 60, log: 120 }
  // One buffer, one shape. `kind` is the difference between "the page complained about itself",
  // "a request failed", "a resource 404'd" and "the page printed something" — which is exactly the
  // distinction a model gets wrong when it is handed a flat list of strings.
  let seq = 0
  cw.di = []
  cw.net = []
  function record(kind, level, text, extra) {
    const e = { seq: ++seq, t: Date.now(), kind, level, text: String(text || '').slice(0, 700) }
    if (extra) for (const k of ['url', 'status', 'method', 'where', 'nav', 'save']) if (extra[k] != null) e[k] = extra[k]
    cw.di.push(e)
    if (cw.di.length > CAP.di) cw.di.splice(0, cw.di.length - CAP.di)
    return e
  }

  function flat(args) {
    return Array.from(args)
      .map((a) => {
        if (a instanceof Error) return (a.stack || a.message || String(a)).split('\n').slice(0, 4).join(' | ')
        if (typeof a === 'object' && a !== null) {
          try {
            return JSON.stringify(a).slice(0, 300)
          } catch {
            return String(a)
          }
        }
        return String(a)
      })
      .join(' ')
  }

  for (const level of ['log', 'info', 'warn', 'error', 'debug']) {
    const orig = console[level] ? console[level].bind(console) : () => {}
    console[level] = (...args) => {
      // A console call is a *message*, whatever its level — including console.error, which is the
      // page choosing to say something in loud type. An exception is a different kind of record.
      const lvl = level === 'error' ? 'error' : level === 'warn' ? 'warning' : level === 'debug' ? 'debug' : 'info'
      record('console', lvl, flat(args))
      orig(...args)
    }
  }

  window.addEventListener(
    'error',
    (e) => {
      // The capture-phase listener sees two different things: an ErrorEvent with a `target` that is
      // an element is a *resource* that failed to load, and one with a message is the page throwing.
      const t = e && e.target
      if (t && t !== window && (t.src || t.href)) {
        const tag = (t.tagName || '').toLowerCase()
        record('resource', 'error', `${tag} could not load`, {
          url: String(t.src || t.href).slice(0, 300),
          method: null,
        })
      } else {
        // Where in the page it happened. A file: page can report an empty filename, and
        // `location.href:line` is still a fact about the throw.
        const at = (file, line, col) => `${String(file || location.href).slice(0, 200)}:${line}:${col}`
        const where = e && e.filename ? at(e.filename, e.lineno, e.colno) : at('', e && e.lineno, e && e.colno)
        const stack = e && e.error && e.error.stack ? String(e.error.stack).split('\n').slice(0, 4).join(' | ') : null
        // A page in an opaque origin — a file: document, a cross-origin script — hands `window.onerror`
        // the one word "Script error." and hides the message. The thrown object is still reachable
        // here, and its message is the one the person would have read in the console, so the object is
        // used when it is there. When it is *not*, "Script error." is a dead end for whoever reads the
        // log: it names no page and no reason, and the reader cannot tell a quiet page from a browser
        // that withheld the sentence. So the record says what was withheld and where to go read it.
        const inner = e && e.error ? String(e.error.message || e.error.name || '') : ''
        const masked = (m) => !m || /^script error\.?$/i.test(String(m).trim())
        const said =
          masked(e && e.message) && inner
            ? inner
            : masked(e && e.message)
              ? 'the page threw something and this browser would not say what: a page opened from a file,'
                + ' or a script from another origin, reports only "Script error.". Open the page over http'
                + ' (or read its console in the page itself) to get the message.'
              : (e && e.message) || 'a page error with no message'
        record('error', 'error', said, {
          where: stack || where,
        })
      }
    },
    true
  )

  window.addEventListener('unhandledrejection', (e) => {
    record('error', 'error', `unhandled promise rejection: ${((e && e.reason) && (e.reason.message || e.reason)) || ''}`)
  })

  const origFetch = window.fetch
  if (typeof origFetch === 'function') {
    window.fetch = function (input, init) {
      const where = typeof input === 'string' ? input : (input && input.url) || ''
      const how = ((init && init.method) || (typeof input === 'object' && input && input.method) || 'GET').toUpperCase()
      const started = Date.now()
      const p = origFetch.apply(this, arguments)
      p.then(
        (res) => {
          note(where, how, res && res.status, Date.now() - started, res && res.ok)
        },
        (err) => {
          note(where, how, 0, Date.now() - started, false, String((err && err.message) || err))
        }
      )
      return p
    }
  }

  const origOpen = XMLHttpRequest.prototype.open
  XMLHttpRequest.prototype.open = function (method, url) {
    this.__cwu = url
    this.__cwm = String(method || 'GET').toUpperCase()
    this.__cwt = Date.now()
    return origOpen.apply(this, arguments)
  }
  const origSend = XMLHttpRequest.prototype.send
  XMLHttpRequest.prototype.send = function () {
    this.addEventListener('loadend', () => {
      note(this.__cwu, this.__cwm, this.status, Date.now() - (this.__cwt || Date.now()), this.status >= 200 && this.status < 400)
    })
    this.addEventListener('error', () => note(this.__cwu, this.__cwm, 0, Date.now() - (this.__cwt || Date.now()), false, 'connection failed'))
    return origSend.apply(this, arguments)
  }

  function note(url, method, status, ms, ok, why) {
    const u = String(url || '').slice(0, 300)
    const entry = { url: u, method: method || 'GET', status: status || 0, ms, ok: !!ok }
    cw.net.push(entry)
    if (cw.net.length > CAP.net) cw.net.shift()
    if (ok) return
    // A 404 on a favicon and a crashed API call are both "network", and the status is what makes
    // them mean different things. Keep the status, never guess at the severity.
    record('network', status >= 400 ? (status >= 500 ? 'error' : 'warning') : 'error', why || `request failed${status ? ' with status ' + status : ''}`, {
      url: u,
      method: method || 'GET',
      status: status || 0,
    })
  }

  // Real input from a real person. The app turns this into activity on the idle clock: somebody
  // typing into the page is using the browser, and the five-minute promise is about that.
  cw.touched = 0
  for (const ev of ['pointerdown', 'keydown', 'wheel']) {
    window.addEventListener(
      ev,
      () => {
        cw.touched++
      },
      { passive: true, capture: true }
    )
  }

  /* ---------------------------------------------------------------- naming things */

  const ROLES = { a: 'link', button: 'button', textarea: 'textbox', h1: 'heading', h2: 'heading', h3: 'heading', h4: 'heading', img: 'img', select: 'combobox' }
  function roleOf(el) {
    const r = el.getAttribute && el.getAttribute('role')
    if (r) return r
    const tag = el.tagName.toLowerCase()
    if (tag === 'input') {
      const type = (el.getAttribute('type') || 'text').toLowerCase()
      if (['text', 'email', 'tel', 'url', 'password', 'search'].includes(type)) return 'textbox'
      if (type === 'checkbox' || type === 'radio') return type
      if (['submit', 'button', 'reset'].includes(type)) return 'button'
      return `input[${type}]`
    }
    return ROLES[tag] || tag
  }

  function nameOf(el) {
    const direct = el.getAttribute && (el.getAttribute('aria-label') || '')
    if (direct) return direct.trim()
    const by = el.getAttribute && el.getAttribute('aria-labelledby')
    if (by) {
      const parts = by
        .split(/\s+/)
        .map((id) => document.getElementById(id))
        .filter(Boolean)
        .map((n) => n.textContent.trim())
      if (parts.length) return parts.join(' ').trim()
    }
    if (el.labels && el.labels.length) return Array.from(el.labels).map((l) => l.textContent.trim()).join(' ').trim()
    const ph = el.getAttribute && el.getAttribute('placeholder')
    if (ph) return ph.trim()
    const alt = el.getAttribute && el.getAttribute('alt')
    if (alt) return alt.trim()
    const title = el.getAttribute && el.getAttribute('title')
    const text = (el.textContent || '').trim().replace(/\s+/g, ' ')
    if (text) return text.slice(0, 120)
    return (title || '').trim()
  }

  function norm(s) {
    return String(s || '').trim().replace(/\s+/g, ' ').toLowerCase()
  }

  function path(el) {
    if (el.id) return '#' + el.id
    const parts = []
    let n = el
    while (n && n.tagName && parts.length < 4) {
      let seg = n.tagName.toLowerCase()
      if (n.className && typeof n.className === 'string') {
        const cls = n.className.trim().split(/\s+/).filter(Boolean).slice(0, 2)
        if (cls.length) seg += '.' + cls.join('.')
      }
      parts.unshift(seg)
      n = n.parentElement
    }
    return parts.join(' > ')
  }

  const INTERACTIVE = 'a,button,input,select,textarea,[role],[tabindex],summary,area,[contenteditable=true],[onclick],label,form'

  // A person names a page by the words on it: "ready", "No results yet", "BOTTOM-MARKER". Neither
  // `querySelectorAll` nor the control-only passes above can answer that — a status line is not a
  // control — so the words of the whole document are searched, and only the *innermost* elements that
  // say it are kept. A card that says "Save" in its heading and on its button must answer with the
  // button rather than "2 elements match", and `<body>` must never be an answer.
  //
  // `loose` is the documented "visible text" spelling: case-insensitive, whitespace-normalised, and
  // allowed to be part of what the element says — which is what `browser/session.mjs`'s `getByText`
  // does, and the two drivers have to name things the same way or the model learns two dialects.
  function byText(q, loose) {
    const want = norm(q)
    if (!want) return []
    const said = Array.from(document.body ? document.body.querySelectorAll('*') : []).filter((el) => {
      const t = norm(el.textContent)
      if (!t) return false
      return loose ? t.includes(want) : t === want
    })
    return said.filter((el) => !said.some((other) => other !== el && el.contains(other)))
  }

  // The same ladder as browser/session.mjs, in the same order, with the same wording for a tie.
  // `any` is for the verbs that only look. Waiting on a thing that has several matches is answered
  // by any of them; a click is not, and refuses.
  function locate(t, opts) {
    const q = String(t || '')
    if (!q) return { err: 'no target named' }
    const all = Array.from(document.querySelectorAll(INTERACTIVE))
    const tries = [
      () => all.filter((el) => norm(nameOf(el)) === norm(q)),
      () => all.filter((el) => norm(el.getAttribute('placeholder')) === norm(q)),
      () => all.filter((el) => ['button', 'link', 'textbox'].includes(roleOf(el)) && norm(nameOf(el)) === norm(q)),
      () => all.filter((el) => norm(el.textContent) === norm(q)),
      () => all.filter((el) => norm(nameOf(el)).includes(norm(q))),
      () => byText(q, false),
      () => byText(q, true),
      () => Array.from(document.querySelectorAll(q)),
    ]
    for (const make of tries) {
      let els
      try {
        els = make()
      } catch {
        continue
      }
      if (els.length === 1) return { el: els[0] }
      if (els.length > 1 && opts && opts.any) return { el: els[0], n: els.length }
      if (els.length > 1) {
        // Say what the candidates *are*. "name it more narrowly" on its own sends a model back to
        // guessing: it cannot write a narrower selector than the page can show it.
        return {
          err:
            `${els.length} elements match ${JSON.stringify(q)}: ${els.slice(0, 4).map(describe).join(' | ')}` +
            (els.length > 4 ? ` (and ${els.length - 4} more)` : '') +
            `. Say which one — an id, a class, a placeholder, or the words in its label. Nothing was touched.`,
        }
      }
    }
    return { err: `nothing on the page matches ${JSON.stringify(q)}` }
  }

  // Enough of an element to write a selector for it: tag, id, first classes, what it holds.
  function describe(el) {
    const tag = el.tagName.toLowerCase()
    const id = el.id ? `#${el.id}` : ''
    const cls =
      typeof el.className === 'string' && el.className.trim()
        ? '.' + el.className.trim().split(/\s+/).slice(0, 2).join('.')
        : ''
    const bits = []
    const ph = el.getAttribute && el.getAttribute('placeholder')
    if (ph) bits.push(`placeholder=${JSON.stringify(ph)}`)
    const nm = norm(nameOf(el))
    if (nm) bits.push(JSON.stringify(nm.slice(0, 40)))
    return `${tag}${id}${cls}${bits.length ? ' (' + bits.join(' ') + ')' : ''}`
  }

  function visible(el) {
    const r = el.getBoundingClientRect()
    const cs = getComputedStyle(el)
    return r.width > 0 && r.height > 0 && cs.visibility !== 'hidden' && cs.display !== 'none' && cs.opacity !== '0'
  }

  /* ---------------------------------------------------------------- acting */

  function paint(el) {
    // A click a page can see. Pages that only style :hover/:focus, or that need a real pointer
    // sequence, do not respond to el.click() alone.
    const r = el.getBoundingClientRect()
    const at = {
      bubbles: true,
      cancelable: true,
      composed: true,
      clientX: Math.round(r.left + r.width / 2),
      clientY: Math.round(r.top + r.height / 2),
      button: 0,
    }
    try {
      el.focus({ preventScroll: true })
    } catch {}
    for (const type of ['pointerover', 'pointerenter', 'pointerdown', 'mouseover', 'mousedown', 'pointerup', 'mouseup', 'click']) {
      const ev = type.startsWith('pointer') ? new PointerEvent(type, at) : new MouseEvent(type, at)
      el.dispatchEvent(ev)
    }
  }

  // Writing a field's value the way *that element* accepts one. The prototype the `value` setter is
  // borrowed from has to match the element: `HTMLInputElement`'s setter called on a `<select>` makes
  // JavaScriptCore answer with its own internals — "The HTMLInputElement.value setter can only be
  // used on instances of HTMLInputElement" — and the model is handed an engine message instead of a
  // sentence about the page.
  function setValue(el, v) {
    const tag = el.tagName
    if (tag === 'SELECT') return selectValue(el, v)
    if (el.isContentEditable) {
      el.textContent = v
    } else {
      const proto = tag === 'TEXTAREA' ? HTMLTextAreaElement.prototype : tag === 'INPUT' ? HTMLInputElement.prototype : null
      const setter = proto && Object.getOwnPropertyDescriptor(proto, 'value')
      if (setter && setter.set) setter.set.call(el, v)
      else el.value = v
    }
    el.dispatchEvent(new Event('input', { bubbles: true }))
    el.dispatchEvent(new Event('change', { bubbles: true }))
    return { ok: true, value: String(el.value != null ? el.value : el.textContent || '') }
  }

  // A `<select>` is not typed into, it is chosen from. The names a model has are the ones a person
  // reads in the list, so an option is matched by its value and then by the words it shows; a name
  // that is not on the list says what the list offers rather than leaving the next guess to the model.
  function selectValue(el, v) {
    const want = norm(v)
    const options = Array.from(el.options || [])
    const hit = options.find((o) => norm(o.value) === want) || options.find((o) => norm(o.textContent) === want)
    if (!hit) {
      const shown = options
        .slice(0, 12)
        .map((o) => (norm(o.textContent) === norm(o.value) ? o.value : `${(o.textContent || '').trim()}=${o.value}`))
        .join(', ')
      return {
        err: `${path(el)} is a choice, not a field. It offers ${options.length} option(s): ${shown || '(none)'}. "${v}" is not one of them, and nothing was changed.`,
      }
    }
    el.value = hit.value
    el.dispatchEvent(new Event('input', { bubbles: true }))
    el.dispatchEvent(new Event('change', { bubbles: true }))
    return { ok: true, value: hit.value, chose: (hit.textContent || '').trim() }
  }

  function keyAt(el, key) {
    const opts = { key, bubbles: true, cancelable: true }
    el.dispatchEvent(new KeyboardEvent('keydown', opts))
    el.dispatchEvent(new KeyboardEvent('keypress', opts))
    el.dispatchEvent(new KeyboardEvent('keyup', opts))
  }

  // Where the caret is, named the way a step can aim at it again. A keyboard test is a test about
  // focus, so every step that can move it says where it landed.
  function focusPath() {
    const el = document.activeElement
    if (!el || el === document.body || el === document.documentElement) return ''
    return path(el)
  }

  const FOCUSABLE = 'a[href],button,input,select,textarea,summary,[tabindex],[contenteditable=true]'

  // The tab order, in the order the page would walk it.
  function tabbables() {
    return Array.from(document.querySelectorAll(FOCUSABLE)).filter((el) => {
      if (el.disabled || el.tabIndex < 0) return false
      return visible(el)
    })
  }

  // Is this element supposed to take us somewhere? A click on a link or a submit is a step whose
  // whole job is navigation, so the app should wait for the move rather than answer "the page is
  // still here" about a document that is on its way out. A `download` link is the exception that
  // proves the rule: it names a destination, but the document stays exactly where it is.
  function mayNavigate(el) {
    if (!el || !el.closest) return false
    const a = el.closest('a[href], area[href]')
    if (a) {
      const href = a.getAttribute('href') || ''
      if (a.hasAttribute && a.hasAttribute('download')) return false
      return href !== '' && !href.startsWith('#')
    }
    if (!el.closest('form')) return false
    if (el.tagName === 'BUTTON') return (el.getAttribute('type') || 'submit').toLowerCase() === 'submit'
    if (el.tagName === 'INPUT') return (el.getAttribute('type') || 'text').toLowerCase() === 'submit'
    return false
  }

  function after() {
    return { url_after: location.href, title: document.title, ready: document.readyState }
  }

  // **A click on a `download` link offers a file; it does not move the page.** It should be the
  // browser's own machinery that saves the file, and in this browser that machinery is silent:
  // WebKitGTK stopped telling its embedder about downloads (the signal wry connects has not fired for
  // an ordinary link since 2.40), so a download link here is just a navigation — the address moves to
  // the file, the page the person was reading is gone, and nothing was ever saved. So cancel the
  // default action of this one click and leave a record saying what was offered; the app reads the
  // record, puts the bytes in the workspace's `.cwoffee/downloads`, and answers in the same log.
  function offer(el) {
    const a = el && el.closest ? el.closest('a[download], area[download]') : null
    if (!a) return null
    const href = a.getAttribute('href') || ''
    let where = href
    try {
      where = new URL(href, location.href).href
    } catch {}
    const stop = (ev) => ev.preventDefault()
    a.addEventListener('click', stop, { capture: true, once: true })
    const name =
      a.getAttribute('download') ||
      where.split('/').pop().split(/[?#]/)[0] ||
      'a file'
    return { stop, where, name }
  }

  // The app calls `__cw.run(req, id)` and the answer is left in `__cwR[id]` for it to poll for.
  // Left there rather than returned through an eval callback because wry drops the callback of an
  // eval issued against a page that is still loading: a lost answer would be a hang, not an error.
  //
  // Most steps answer in the tick they were asked. The ones that cannot — `wait`, `wait_for`, an
  // `eval` that handed back a promise — return `LATE` and write the answer when they are ready. That
  // is what makes those verbs real here: a step that cannot answer yet and does not answer at all is
  // the difference between "the page is slow" and "the step did nothing", and the transcript from the
  // person's own testing could not tell them apart.
  const LATE = { __late: true }
  cw.seq = 0
  function deliver(key, answer) {
    window.__cwR[key] = answer
  }

  // How long a step that has to answer *later* may take, in the page's own terms: the caller's budget
  // for this step with a margin left to write the answer inside it. A `wait(30000)` inside a step the
  // caller will give up on in 20 seconds is not a 30-second wait, and the honest answer says so
  // instead of timing out with nothing to show.
  function roomMs(req) {
    const given = Number(req.timeout_ms)
    if (Number.isFinite(given) && given > 0) return Math.max(200, Math.min(given - 400, 30_000))
    return 30_000
  }

  cw.run = function (req, id) {
    const key = String(id || 'a' + ++cw.seq)
    delete window.__cwR[key]
    let answer
    try {
      answer = perform(req || {}, key)
    } catch (e) {
      answer = { ok: false, why: String((e && e.message) || e).split('\n')[0] }
    }
    if (answer && answer.__late) return key
    window.__cwR[key] = answer
    return key
  }

  // One step: click, fill, press, scroll, read. The verbs and their arguments are the ones
  // browser/session.mjs answers to, so the model is told the same things about a page whichever
  // browser it is looking at.
  function perform(req, key) {
    // `do` is the verb key the hub and the browse tool use; `verb` is accepted too so a hand
    // written job is not silently a no-op.
    const what = String(req.do || req.verb || '')
    switch (what) {
        case 'open':
        case 'navigate': {
          const url = String(req.url || '')
          if (!url) return { ok: false, why: 'no url to open' }
          location.href = url
          // The step that asks for a new page is the step the app has to wait on: the answer it can
          // give right now is about the page being left, and `may_navigate` is how the app is told to
          // re-read once the new document is up.
          return { ok: true, may_navigate: true, ...after() }
        }
        case 'click': {
          const l = locate(req.target)
          if (l.err) return { ok: false, why: l.err }
          if (!visible(l.el)) return { ok: false, why: `${path(l.el)} is on the page but not visible, so a click there would be a guess` }
          const off = offer(l.el)
          paint(l.el)
          if (off) record('download', 'info', `the page offered ${off.name}`, { url: off.where, save: true })
          // A download is not a page: the click that starts one leaves the document alone, so the app
          // must not wait for a document that is never coming. And whether the file arrived is not
          // something the page knows — the app's own records do — so the step says where to ask
          // instead of leaving "ok" to mean "the download happened".
          const dl = l.el.closest && l.el.closest('[download]')
          return {
            ok: true,
            hit: path(l.el),
            focus: focusPath(),
            may_navigate: mayNavigate(l.el),
            ...(dl
              ? { note: `this is a download link (${dl.getAttribute('download') || dl.getAttribute('href') || 'a file'}); the page stays where it is, and page_log with kind "download" says where cwoffee saved it` }
              : {}),
            ...after(),
          }
        }
        case 'click_xy': {
          const x = Number(req.x)
          const y = Number(req.y)
          if (!Number.isFinite(x) || !Number.isFinite(y)) return { ok: false, why: 'click_xy needs x and y in page pixels' }
          const el = document.elementFromPoint(x, y)
          if (!el) return { ok: false, why: `there is nothing at ${Math.round(x)},${Math.round(y)} on this page` }
          const off = offer(el)
          paint(el)
          if (off) record('download', 'info', `the page offered ${off.name}`, { url: off.where, save: true })
          return { ok: true, hit: path(el), may_navigate: mayNavigate(el), ...after() }
        }
        case 'fill': {
          const l = locate(req.target)
          if (l.err) return { ok: false, why: l.err }
          const el = l.el
          if (el.disabled) return { ok: false, why: `${path(el)} is disabled` }
          if (!('value' in el) && !el.isContentEditable) return { ok: false, why: `${path(el)} is not a field; it is a ${roleOf(el)}` }
          el.focus()
          const wrote = setValue(el, String(req.text ?? ''))
          if (wrote && wrote.err) return { ok: false, why: wrote.err }
          return { ok: true, hit: path(el), focus: focusPath(), ...(wrote && wrote.chose ? { chose: wrote.chose } : {}), ...after() }
        }
        case 'type_keys': {
          // Typed into whatever already has focus, which is what a person aiming with a mouse does
          // after clicking a box. There is no selector to name, and none is needed.
          const t = String(req.text ?? '')
          if (!t) return { ok: false, why: 'type_keys needs text' }
          if (t.length > 2000) return { ok: false, why: 'type_keys takes at most 2000 characters at a time' }
          const el = document.activeElement
          if (!el || el === document.body) return { ok: false, why: 'nothing in the page has focus to type into' }
          for (const ch of t) {
            keyAt(el, ch)
            if ('value' in el) {
              const pos = typeof el.selectionStart === 'number' ? el.selectionStart : el.value.length
              el.value = el.value.slice(0, pos) + ch + el.value.slice(pos)
              el.dispatchEvent(new Event('input', { bubbles: true }))
            }
          }
          el.dispatchEvent(new Event('change', { bubbles: true }))
          return { ok: true, hit: path(el), ...after() }
        }
        case 'press': {
          const named = String(req.key ?? req.text ?? '')
          if (!named) return { ok: false, why: 'press needs a key' }
          const shift = named.startsWith('Shift+')
          const key = shift ? named.slice(6).trim() : named
          const el = document.activeElement && document.activeElement !== document.body ? document.activeElement : null
          const said = (extra) => ({
            ok: true,
            key: named,
            focus: focusPath(),
            // Whether the page actually has the window's focus. A key pressed by a synthetic event
            // reaches the page's own handlers either way; what it cannot do is a browser default.
            // Saying which of the two happened is the difference between "the page ignores Tab" and
            // "this page never had the keyboard".
            ...(document.hasFocus() ? {} : { note: 'the page does not have the window focus: its own key handlers ran, and no browser default with them' }),
            ...extra,
            ...after(),
          })
          if (key === 'Tab') {
            // A real Tab moves focus, and that is a browser action a synthetic key event cannot
            // cause. So the move is done here, in the order the page would walk it, and the page's
            // own Tab handlers still see the key.
            const list = tabbables()
            if (!list.length) return { ok: false, why: 'nothing in this page can take focus, so Tab has nowhere to go', key: named, ...after() }
            const at = el ? list.indexOf(el) : -1
            const step = shift ? -1 : 1
            const next = list[(((at === -1 ? 0 : at) + step) % list.length + list.length) % list.length]
            try {
              next.focus()
            } catch {}
            keyAt(el || document.body, 'Tab')
            return said({ moved_focus: true, from: el ? path(el) : '', ...(at === -1 ? { note: 'nothing was focused, so Tab went to the first control in the page' } : {}) })
          }
          const target = el || document.body
          keyAt(target, key)
          // What a real key would also have *done*. Enter in a form submits it, Enter on a button
          // presses it, Space toggles a checkbox: a page's script sees the key either way, and the
          // model testing a form needs the default action the person would have got.
          const form = target.closest && target.closest('form')
          const type = (target.getAttribute && (target.getAttribute('type') || '').toLowerCase()) || ''
          if (key === 'Enter' && form && target.tagName !== 'TEXTAREA' && target.tagName !== 'BUTTON') {
            if (typeof form.requestSubmit === 'function') form.requestSubmit()
            else form.dispatchEvent(new Event('submit', { bubbles: true, cancelable: true }))
            return said({ did: `submitted ${path(form)}`, may_navigate: true })
          }
          if (key === 'Enter' && target.tagName === 'BUTTON') {
            paint(target)
            return said({ did: `pressed ${path(target)}`, may_navigate: mayNavigate(target) })
          }
          if (key === ' ' && target.tagName === 'INPUT' && (type === 'checkbox' || type === 'radio')) {
            target.checked = type === 'radio' ? true : !target.checked
            target.dispatchEvent(new Event('input', { bubbles: true }))
            target.dispatchEvent(new Event('change', { bubbles: true }))
            return said({ did: `${path(target)} is now ${target.checked ? 'on' : 'off'}` })
          }
          return said({})
        }
        case 'scroll': {
          const px = Number(req.px ?? 600)
          if (Number.isFinite(Number(req.x)) && Number.isFinite(Number(req.y))) {
            const el = document.elementFromPoint(Number(req.x), Number(req.y))
            if (el && el !== document.body) {
              el.scrollBy({ top: px, behavior: 'instant' })
              return { ok: true, scrolled: path(el), y: Math.round(scrollY), ...after() }
            }
          }
          window.scrollBy({ top: px, behavior: 'instant' })
          return { ok: true, y: Math.round(scrollY), ...after() }
        }
        case 'wait_for': {
          // Looking is not touching. Several matches do not make "has this appeared?" ambiguous the
          // way they make a click dangerous, so the first one answers and the count travels with it.
          //
          // And it *keeps looking* for the time the step was given. A single check is not a wait: the
          // thing being waited for is usually the thing the page has not done yet, and a step that
          // answers in 3ms about a 4-second wait tells the model the page failed at its own job.
          const room = roomMs(req)
          const want = Math.max(0, Math.min(Number(req.ms ?? room), room))
          const started = Date.now()
          let lastWhy = `nothing on the page matches ${JSON.stringify(String(req.target ?? ''))}`
          const look = () => {
            const l = locate(req.target, { any: true })
            if (!l.err) {
              if (visible(l.el)) {
                deliver(key, {
                  ok: true,
                  hit: path(l.el),
                  waited: Date.now() - started,
                  ...(l.n > 1 ? { note: `${l.n} things matched, and the first one is here` } : {}),
                  ...after(),
                })
                return
              }
              lastWhy = `${path(l.el)} is on the page but not visible`
            } else lastWhy = l.err
            const waited = Date.now() - started
            if (waited >= want) {
              deliver(key, {
                ok: false,
                why: want > 0 ? `${lastWhy} — looked for ${waited}ms and it never appeared` : `${lastWhy} (and this step asked for no time to look)`,
                waited,
                ...after(),
              })
              return
            }
            setTimeout(look, 60)
          }
          look()
          return LATE
        }
        case 'wait': {
          // The page's own clock, not the caller's hope of one. Answering with the number that was
          // asked for is how a step that did nothing at all reads as a step that waited: every
          // timer, animation and async result on the page is still in flight when the next step
          // reads it, and the report blames the page.
          const want = Math.max(0, Math.min(Number(req.ms ?? 500), 30_000))
          const room = roomMs(req)
          const took = Math.min(want, room)
          const started = Date.now()
          setTimeout(() => {
            const waited = Date.now() - started
            deliver(key, {
              ok: true,
              waited,
              ...(took < want ? { note: `this step only had ${room}ms to answer in, so it waited ${waited}ms of the ${want}ms asked for` } : {}),
              ...after(),
            })
          }, took)
          return LATE
        }
        case 'eval': {
          const code = String(req.code ?? 'document.title')
          // The completion value, the way page.evaluate treats a string: an expression gives its
          // value, a run of statements gives the value of the last one. Wrapping in a function and
          // hoping for the best returns undefined for every plain expression.
          const value = window.eval(code)
          if (value && typeof value.then === 'function') {
            // The page handed back a promise, so the answer is what it settles to. Stringifying a
            // pending promise is `{}`, which reads as "the page said an empty object" — and the
            // model has no way to tell that from "the page said nothing".
            const room = roomMs(req)
            const started = Date.now()
            let settled = false
            const done = (answer) => {
              if (settled) return
              settled = true
              deliver(key, answer)
            }
            value.then(
              (v) => done({ ok: true, value: text_of(v), awaited: Date.now() - started, ...after() }),
              (e) => done({ ok: false, why: `the promise the page returned rejected: ${String((e && e.message) || e).split('\n')[0]}`, awaited: Date.now() - started, ...after() })
            )
            setTimeout(() => done({ ok: false, why: `the promise the page returned had not settled within ${room}ms`, awaited: Date.now() - started, ...after() }), room)
            return LATE
          }
          return { ok: true, value: text_of(value) }
        }
        case 'text':
          return { ok: true, value: (document.body ? document.body.innerText : '').slice(0, 24000), truncated: (document.body ? document.body.innerText.length : 0) > 24000 }
        case 'snapshot':
          return { ok: true, value: cw.a11y(120).items.map(line).join('\n') || 'this page has no controls to describe', ...after() }
        case 'a11y': {
          // Text out, always. The app prints a step's `value` as the words it gave back, and a step
          // that answers with an array of objects is a step that reports `ok` and then says nothing —
          // which reads to the model as "the page has no elements" rather than "this driver answered
          // in a shape nobody renders". `max` is the name in the tool's schema; `limit` is taken too.
          const want = Math.min(Number(req.max ?? req.limit ?? 120), 400)
          const r = cw.a11y(want)
          return {
            ok: true,
            value: r.items.length ? r.items.map(line).join('\n') : 'nothing visible in this page to describe',
            total: r.total,
            truncated: r.truncated,
            ...after(),
          }
        }
        case 'health': {
          // Same rule as `a11y`: one sentence a reader can act on, with the numbers in it. The counts
          // come from *this document's* own records, not from every page the session has visited.
          const h = health()
          const c = h.counts
          const bits = [
            `${c.console || 0} console message(s)`,
            `${c.error || 0} uncaught error(s)`,
            `${c.network || 0} failed request(s)`,
            `${c.resource || 0} resource(s) that did not load`,
          ]
          const worst = (h.failed || []).slice(-4).map((f) => `${f.kind}: ${f.text}${f.url ? ` (${f.url})` : ''}`).join(' | ')
          return {
            ok: true,
            value:
              `${h.ready} at ${h.url}${h.title ? ` — "${h.title}"` : ''}: ` +
              bits.join(', ') +
              (worst ? `; the last things that went wrong: ${worst}` : '; nothing has gone wrong on this page'),
            counts: c,
            failed: (h.failed || []).length,
            touched: h.touched,
            ...after(),
          }
        }
        case 'diagnostics': {
          const items = cw.poll(0).items
          return {
            ok: true,
            value: items.length ? items.map((e) => `#${e.seq} ${e.kind}${e.level && e.level !== 'info' ? ` ${e.level}` : ''}: ${e.text}${e.url ? ` (${e.url})` : ''}`).join('\n') : 'this page has said nothing since it loaded',
            total: items.length,
            ...after(),
          }
        }
        // History walks, and a `reload`. The move is asked for and answered in the same tick, with
        // `may_navigate` set, because the app is the one that can tell whether a *new document*
        // arrived: it compares the page's boot id, waits for the one that is coming, and re-reads the
        // address. Answering from inside the page after the move cannot work — when the move makes a
        // new document, an answer left waiting in the old one is destroyed with it, and the app spends
        // its whole step budget polling a page that no longer exists. That is how a Back button that
        // works gets reported as "the browser did not answer that command within 20s".
        case 'back':
          history.back()
          return { ok: true, may_navigate: true, moved: 'back', ...after() }
        case 'forward':
          history.forward()
          return { ok: true, may_navigate: true, moved: 'forward', ...after() }
        case 'reload':
          location.reload()
          return { ok: true, may_navigate: true, moved: 'reload', ...after() }
        case 'stop': {
          // "Stop loading" means what it says: the pending navigation is dropped, and the page goes
          // back to being interactive.
          window.stop()
          return { ok: true, stopped: true, ...after() }
        }
        default:
          return { ok: false, why: `I do not know the act "${what}"` }
      }
  }

  function line(it) {
    return `${it.role}${it.id ? ' #' + it.id : ''}${it.name ? ` "${it.name}"` : ''}${it.value ? ` = ${it.value}` : ''}${it.disabled ? ' (disabled)' : ''}`
  }

  // An answer's text, whatever the page handed back. `undefined` and a function say so instead of
  // becoming an empty string, which is the shape "the page said nothing" already has.
  function text_of(v) {
    if (v === undefined) return '(undefined)'
    if (typeof v === 'function') return '(a function)'
    let s
    try {
      s = typeof v === 'string' ? v : JSON.stringify(v)
    } catch {
      s = String(v)
    }
    return String(s ?? '').slice(0, 24000)
  }

  // The accessibility-shaped view of the page: what a person calling out the page would hear, with
  // the selector the model needs to aim at it. This is the answer to "inspect the DOM" that is not
  // a wall of markup.
  cw.a11y = function (limit) {
    const all = Array.from(document.querySelectorAll(INTERACTIVE))
    const items = []
    let total = 0
    for (const el of all) {
      if (!visible(el)) continue
      total++
        if (items.length >= limit) continue
      const r = el.getBoundingClientRect()
      const item = {
        role: roleOf(el),
        tag: el.tagName.toLowerCase(),
        name: nameOf(el).slice(0, 120),
        selector: el.id ? '#' + el.id : path(el),
      }
      if (el.id) item.id = el.id
      if ('value' in el && el.type !== 'password') item.value = String(el.value ?? '').slice(0, 120)
      if (el.disabled) item.disabled = true
      if (el.href) item.href = String(el.href).slice(0, 300)
      if (el.getAttribute('aria-checked') != null) item.checked = el.getAttribute('aria-checked')
      item.rect = { x: Math.round(r.x), y: Math.round(r.y), w: Math.round(r.width), h: Math.round(r.height) }
      items.push(item)
    }
    return { items, total, truncated: total > items.length }
  }

  // One answer to "is this page all right": counts by kind, the failures, and what the last
  // navigation was. Assembled from the same buffer the panel shows, so the person and the model
  // cannot be looking at different numbers.
  function health() {
    const counts = { console: 0, error: 0, network: 0, resource: 0 }
    for (const e of cw.di) counts[e.kind] = (counts[e.kind] || 0) + 1
    const fails = cw.di.filter((e) => e.kind === 'network' || e.kind === 'resource' || e.kind === 'error').slice(-12)
    return {
      url: location.href,
      title: document.title,
      ready: document.readyState,
      counts,
      failed: fails,
      recent_requests: cw.net.slice(-12),
      touched: cw.touched,
    }
  }

  // What the app reads on its own timer. `since` is the app's cursor, so a page that produced
  // nothing costs one tiny answer, and a page that produced forty things costs forty records once.
  cw.poll = function (since) {
    const from = Number(since) || 0
    const items = cw.di.filter((e) => e.seq > from)
    return {
      items,
      cursor: seq,
      boot: cw.boot,
      url: location.href,
      title: document.title,
      touched: cw.touched,
      ready: document.readyState,
      counts: { di: cw.di.length, net: cw.net.length },
      size: { w: innerWidth, h: innerHeight },
    }
  }

  cw.state = function () {
    return {
      url: location.href,
      title: document.title,
      boot: cw.boot,
      ready: document.readyState,
      touched: cw.touched,
      seq,
      // The page's own box. A page that reports 1x1 is a widget nobody mapped, which is a
      // different bug from a page that will not run its script, and the numbers say which.
      size: { w: innerWidth, h: innerHeight },
    }
  }

  cw.__cwDone = cw.done
  window.__cwDone = cw.done
})()
