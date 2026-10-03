<script lang="ts">
  import { onMount, onDestroy } from 'svelte'
  import { invoke, Channel } from '@tauri-apps/api/core'
  import { getCurrentWindow } from '@tauri-apps/api/window'
  import type {
    ApprovalAnswer,
    EngineEvent,
    GpuReport,
    HostState,
    SearchHit,
    Session,
  } from './lib/events'
  import { Rolling, statsFromTurn, fmt } from './lib/live'
  import { Transcript, linkProbe } from './lib/transcript'
  import { pageLog } from './lib/pagelog'

  // ---- state --------------------------------------------------------------
  let rootEl: HTMLDivElement | undefined = $state()
  let transcript: Transcript | null = null

  let busy = $state(false)
  let model = $state('')
  let draft = $state('')
  let mode = $state<'ask' | 'session' | 'auto'>('ask')
  let thinking = $state<'off' | 'low' | 'medium' | 'high' | 'xhigh'>('xhigh')
  let hostState = $state<HostState | null>(null)

  let sessions = $state<Session[]>([])
  let query = $state('')
  let hits = $state<SearchHit[]>([])
  let searchErr = $state('')

  let approval = $state<{ id: number; tool: string; risk: string; summary: string } | null>(null)
  // A turn blocked on a human and a turn still computing look identical unless the
  // UI says otherwise: same disabled button, same spinner-ish label. That is how a
  // web_search that was simply waiting for approval read as "stuck".
  let approvalDeadline = $state(0)
  let approvalLeft = $state(0)
  let tick: number | undefined = undefined
  let editingId = $state<number | null>(null)
  let selectMode = $state(false)
  let selected = $state<Set<number>>(new Set())
  let confirmBulkDelete = $state(false)

  function toggleSelect(id: number) {
    const next = new Set(selected)
    if (next.has(id)) next.delete(id)
    else next.add(id)
    selected = next
    // Any change to the selection invalidates a pending confirmation.
    confirmBulkDelete = false
  }

  function selectAll() {
    selected = new Set(sessions.map((s) => s.id))
    confirmBulkDelete = false
  }

  async function doBulkDelete() {
    const ids = [...selected]
    if (!ids.length) return
    try {
      const n = await invoke<number>('delete_sessions', { ids })
      pageLog(`deleted ${n} session(s)`)
    } catch (e) {
      pageLog(`delete_sessions failed: ${e}`)
    }
    selected = new Set()
    confirmBulkDelete = false
    await loadSessions()
    await refreshState()
  }
  let editTitle = $state('')
  let confirmDeleteId = $state<number | null>(null)
  let sidebarOpen = $state(false)
  let models = $state<string[]>([])

  // Custom popovers instead of <select>: native dropdowns render with the
  // toolkit's own chrome, which cannot be themed, and they look nothing like the
  // rest of the app.
  type MenuKind = 'mode' | 'model' | 'think' | 'row' | 'keys'
  // Anchored in viewport coordinates rather than to the opener, so no ancestor
  // overflow can clip it. `place` records which side it was put on.
  type Menu = {
    kind: MenuKind
    sessionId?: number
    left: number
    right: number
    top: number
    bottom: number
  }
  let menu = $state<Menu | null>(null)
  let menuPos = $state<{ x: number; y: number } | null>(null)
  let menuEl = $state<HTMLElement | null>(null)
  // Genuinely unread: a session whose stored message count grew since it was last
  // opened, and which is not the session currently on screen.
  let unread = $state<Set<number>>(new Set())
  let seenCounts: Record<number, number> = {}
  const MODES = ['ask', 'session', 'auto'] as const
  const SHORTCUTS = [
    { keys: 'Ctrl K', label: 'Focus the composer' },
    { keys: 'Ctrl Enter', label: 'Send' },
    { keys: 'Ctrl N', label: 'New session' },
    { keys: 'Ctrl B', label: 'Toggle sidebar' },
    { keys: 'Alt 1-9', label: 'Open session by position' },
    { keys: 'Enter', label: 'Send (Shift+Enter newline)' },
    { keys: 'y / a / n', label: 'Approve / allow session / deny' },
    { keys: 'Esc', label: 'Close a menu' },
  ]

  const THINKS = ['off', 'low', 'medium', 'high', 'xhigh'] as const
  const MODE_LABEL: Record<'ask' | 'session' | 'auto', string> = {
    ask: 'Ask',
    session: 'Auto for session',
    auto: 'Full auto',
  }
  const MODE_HINT: Record<'ask' | 'session' | 'auto', string> = {
    ask: 'prompt for every mutating call',
    session: 'first allow per tool is remembered',
    auto: 'no prompts; denylist still applies',
  }
  const THINK_LABEL: Record<'off' | 'low' | 'medium' | 'high' | 'xhigh', string> = {
    off: 'Off',
    low: 'Low',
    medium: 'Medium',
    high: 'High',
    xhigh: 'Extra high',
  }
  function titleOf(id: number) {
    return sessions.find((x) => x.id === id)?.title ?? 'session'
  }

  function openMenu(kind: MenuKind, anchor: HTMLElement, sessionId?: number) {
    if (menu && menu.kind === kind && menu.sessionId === sessionId) {
      menu = null
      return
    }
    // Opening the model list is a natural retry point: if the endpoint was down
    // when the app started, this is the moment the user is asking about it.
    if (kind === 'model' && (modelErr || !models.length)) void loadModels()
    const r = anchor.getBoundingClientRect()
    menu = {
      kind,
      sessionId,
      left: r.left,
      right: r.right,
      top: r.top,
      bottom: r.bottom,
    }
  }

  // The menu is rendered invisible first, measured, then placed. Guessing the
  // height is what left it floating 260px above the composer with a visible gap.
  // Prefer below the opener; flip above only when it genuinely does not fit.
  $effect(() => {
    const m = menu
    const el = menuEl
    if (!m || !el) return
    const w = el.offsetWidth
    const h = el.offsetHeight
    const vw = window.innerWidth
    const vh = window.innerHeight
    let x = m.kind === 'mode' ? m.left : m.right - w
    x = Math.max(8, Math.min(x, vw - w - 8))
    let y = m.bottom + 6
    if (y + h + 8 > vh) {
      const above = m.top - h - 6
      y = above >= 8 ? above : Math.max(8, vh - h - 8)
    }
    menuPos = { x, y }
  })

  function closeMenu() {
    menu = null
    menuPos = null
    confirmDeleteId = null
  }

  // Re-run the placement effect (it reads `menu`) without changing anything else.
  function reposition() {
    if (menu) menu = { ...menu }
  }

  function renameFromMenu(id: number) {
    const t = titleOf(id)
    menu = null
    confirmDeleteId = null
    startRename(id, t)
  }

  async function removeFromMenu(id: number) {
    menu = null
    confirmDeleteId = null
    await removeSession(id)
  }

  const SEEN_KEY = 'cowry.seenCounts.v1'
  function loadSeen() {
    try {
      const raw = localStorage.getItem(SEEN_KEY)
      if (raw) seenCounts = JSON.parse(raw)
    } catch {
      /* a corrupt entry is not worth failing the session list over */
    }
  }
  function saveSeen() {
    try {
      localStorage.setItem(SEEN_KEY, JSON.stringify(seenCounts))
    } catch {
      /* non-fatal */
    }
  }

  function applyUnread(list: Session[]) {
    const active = hostState?.session_id ?? 0
    const next = new Set<number>()
    for (const s of list) {
      const seen = seenCounts[s.id]
      // First sight of a session is not unread; only growth since it was read is.
      if (seen === undefined) continue
      if (s.id !== active && s.message_count > seen) next.add(s.id)
    }
    unread = next
  }

  function recordSeen(list: Session[]) {
    const active = hostState?.session_id ?? 0
    const s = list.find((x) => x.id === active)
    if (s) {
      seenCounts[active] = s.message_count
      saveSeen()
      if (unread.delete(active)) unread = new Set(unread)
    }
  }

  function shortModel(m: string) {
    if (!m) return 'no model'
    return m.length > 22 ? m.slice(0, 21) + '\u2026' : m
  }
  async function pickMode(m: 'ask' | 'session' | 'auto') {
    menu = null
    await setMode(m)
  }
  async function pickThinking(t: 'off' | 'low' | 'medium' | 'high' | 'xhigh') {
    menu = null
    await setThinking(t)
  }
  async function pickModel(m: string) {
    menu = null
    await setModel(m)
  }
  async function stopTurn() {
    try {
      await invoke('cancel_turn')
      pageLog('cancel requested')
    } catch (e) {
      pageLog(`cancel_turn failed: ${e}`)
    }
  }

  let gpu = $state<GpuReport | null>(null)
  // IPC health is surfaced in the footer. An empty catch here once hid a CSP
  // misconfiguration that killed every invoke, and every other gate still passed.
  let ipc = $state<{ ok: boolean; msg: string }>({ ok: false, msg: 'not tried' })

  // ---- telemetry ----------------------------------------------------------
  const rolling = new Rolling(10)
  let curDecode = $state(0)
  let avgDecode = $state(0)
  let curPrefill = $state(0)
  let avgPrefill = $state(0)
  let cacheHit = $state(0)
  let cachedTokens = $state(0)
  let promptTokens = $state(0)
  let specPct = $state(0)
  let ttft = $state(0)

  // ---- jank meter ---------------------------------------------------------
  // Kept from M0: it is the only number that tells us the streaming path is
  // still cheap after the UI got real content.
  let frames = $state(0)
  let dropped = $state(0)
  let worstFrame = $state(0)
  let rafId = 0
  let lastFrameTs = 0

  function watchFrames(ts: number) {
    if (lastFrameTs) {
      const dt = ts - lastFrameTs
      frames++
      if (dt > worstFrame) worstFrame = dt
      if (dt > 33) dropped++
    }
    lastFrameTs = ts
    rafId = requestAnimationFrame(watchFrames)
  }
  function stopFrames() {
    if (rafId) cancelAnimationFrame(rafId)
    rafId = 0
  }

  // ---- event handling -----------------------------------------------------
  // Counted so the self-test can tell "the command was accepted" apart from
  // "the UI actually reacted", which is the difference between a host bug and a
  // front-end bug when a session click appears to do nothing.
  let sessionChangedCount = 0
  // Which session the *view* shows, and which session the in-flight turn
  // belongs to. They are different things, and conflating them caused two of
  // the reported bugs: clicking a session during a turn did nothing (the view
  // was waiting on a host command that was queued behind the turn), and the
  // running turn's reasoning block was painted into whatever session happened
  // to be on screen, which read as "this conversation lost its history".
  let viewId = $state<number | null>(null)
  let turnSessionId = $state<number | null>(null)
  // Events of the in-flight turn, kept so returning to its session restores the
  // partial answer instead of showing a truncated one. Bounded: a runaway stream
  // must not grow this without limit.
  let liveBuf: EngineEvent[] = []
  let sentDraft: { text: string; atts: { mime: string; name: string; bytes: number }[] } | null = null
  const LIVE_CAP = 4000
  let eventTaps: ((e: EngineEvent) => void)[] = []
  function beginEventTap(fn: (e: EngineEvent) => void) {
    eventTaps.push(fn)
    return () => {
      eventTaps = eventTaps.filter((t) => t !== fn)
    }
  }

  function handle(ev: EngineEvent) {
    for (const t of eventTaps) t(ev)

    // Live output belongs to the session that started the turn. Buffer it when
    // the user is looking elsewhere, and replay it when they come back.
    const inFlight = turnSessionId !== null && ev.kind !== 'sessionChanged'
    if (inFlight) {
      // Buffer every event of the running turn, not just the ones that arrive
      // while the user is looking elsewhere. Buffering only the latter lost the
      // output that had already been painted before the switch, so coming back
      // showed an empty session - the very thing this is meant to prevent.
      if (liveBuf.length < LIVE_CAP) liveBuf.push(ev)
      if (viewId !== turnSessionId) {
        // painted on return, not now
      } else {
        transcript?.apply(ev)
      }
    } else {
      transcript?.apply(ev)
    }

    switch (ev.kind) {
      case 'sessionChanged':
        sessionChangedCount++
        // The host's switch only repaints the view when it matches what the
        // user is already looking at. Otherwise a queued switch that lands
        // after a turn would yank the transcript out from under them.
        if (ev.d.session_id === viewId) {
          void (async () => {
            try {
              await loadTranscript(ev.d.session_id)
            } catch (e) {
              pageLog(`session_messages after switch failed: ${e}`)
            }
            await refreshState()
            await loadSessions()
          })()
        }
        break
      case 'turnStart':
        model = ev.d.model
        if (turnSessionId === null) turnSessionId = viewId
        busy = true
        frames = 0
        dropped = 0
        worstFrame = 0
        lastFrameTs = 0
        rafId = requestAnimationFrame(watchFrames)
        pageLog(`turn ${ev.d.turn_id} start`)
        break
      case 'toolCallBegin':
        pageLog(`tool begin: ${ev.d.name}`)
        break
      case 'toolExecEnd':
        pageLog(`tool end: ok=${ev.d.ok} exit=${ev.d.exit_code}`)
        break
      case 'approvalRequested':
        approval = { id: ev.d.id, tool: ev.d.tool, risk: ev.d.risk, summary: ev.d.summary }
        const secs = hostState?.approval_timeout_secs ?? 300
        approvalDeadline = Date.now() + secs * 1000
        approvalLeft = secs
        if (tick === undefined) {
          tick = window.setInterval(() => {
            approvalLeft = Math.max(0, Math.round((approvalDeadline - Date.now()) / 1000))
          }, 1000)
        }
        pageLog(`approval ${ev.d.id}: ${ev.d.summary}`)
        break
      case 'usage': {
        const s = statsFromTurn(ev.d)
        rolling.push(s)
        curPrefill = s.effectivePrefillTps
        avgPrefill = rolling.avgRawPrefillTps()
        avgDecode = rolling.avgDecodeTps()
        cacheHit = s.cacheHitPct
        cachedTokens = ev.d.cached_tokens
        promptTokens = ev.d.prompt_tokens
        specPct = s.specAcceptPct
        ttft = ev.d.ttft_ms
        // Prove the bar is populated rather than assume it: these are the exact
        // values the footer binds.
        pageLog(
          `telemetry: cache=${s.cacheHitPct}% (${ev.d.cached_tokens}/${ev.d.prompt_tokens}) ` +
            `prefill=${s.effectivePrefillTps}/s avg=${avgPrefill}/s decode=${s.decodeTps}/s ` +
            `avg=${avgDecode}/s spec=${s.specAcceptPct}% ttft=${ev.d.ttft_ms}ms`,
        )
        break
      }
      case 'turnEnd':
        if (viewId === turnSessionId) transcript?.flushNow()
        busy = false
        turnSessionId = null
        liveBuf = []
        sentDraft = null
        approval = null
        if (tick !== undefined) {
          window.clearInterval(tick)
          tick = undefined
        }
        stopFrames()
        pageLog(
          `turn end chars=${transcript?.streamedChars ?? 0} frames=${frames} ` +
            `dropped=${dropped} worst=${worstFrame.toFixed(0)}ms`,
        )
        pageLog(`answer html: ${transcript?.debugLastAnswerHtml() ?? ''}`)
        void refreshState()
        void loadSessions()
        break
      case 'error':
        if (ev.d.fatal) pageLog(`fatal: ${ev.d.message}`)
        break
    }
  }

  async function attachChannel() {
    const ch = new Channel<EngineEvent>()
    ch.onmessage = handle
    try {
      await invoke('attach', { onEvent: ch })
      ipc = { ok: true, msg: 'ok' }
    } catch (e) {
      ipc = { ok: false, msg: String(e).slice(0, 140) }
      pageLog(`attach FAILED: ${String(e)} | internals=${typeof (window as any).__TAURI_INTERNALS__}`)
    }
  }

  // ---- attachments --------------------------------------------------------
  // Mirrors the engine's guardrails. Duplicated on purpose: a UI-side check
  // gives an instant message, and the engine still refuses the same input if it
  // arrives from anywhere else.
  const MAX_ATTACH = 4
  const MAX_ATTACH_BYTES = 2 * 1024 * 1024
  const OK_MIME = ['image/png', 'image/jpeg', 'image/webp', 'image/gif']

  type Pending = { name: string; mime: string; bytes: number; b64: string; url: string }
  let pending = $state<Pending[]>([])
  let fileInput = $state<HTMLInputElement | null>(null)
  let composerEl = $state<HTMLTextAreaElement | null>(null)
  let dropping = $state(false)
  let wsBusy = $state(false)
  let shortWorkspace = $state('')
  let theme = $state<'light' | 'dark'>('light')
  let attachErr = $state('')

  const THEME_KEY = 'cowry.theme.v1'

  // Explicit choice wins; otherwise follow the OS. Applied to the document
  // element rather than a class on a component, because the palette lives on
  // :root and every utility reads it from there.
  function applyTheme(t: 'light' | 'dark') {
    theme = t
    document.documentElement.dataset.theme = t
    try {
      localStorage.setItem(THEME_KEY, t)
    } catch {
      /* non-fatal */
    }
  }

  function initTheme() {
    let saved: string | null = null
    try {
      saved = localStorage.getItem(THEME_KEY)
    } catch {
      /* non-fatal */
    }
    const t = saved === 'light' || saved === 'dark'
      ? saved
      : matchMedia('(prefers-color-scheme: dark)').matches
        ? 'dark'
        : 'light'
    applyTheme(t)
  }

  // Paste is the path people actually reach for with screenshots, and drag in
  // from a file manager is the other one. Both feed the same guardrails as the
  // picker rather than a second, laxer copy.
  // The engine is the authority on what a workspace may be: it refuses a
  // non-directory, a non-writable one, and any change while a turn is running.
  // The browser preview has no Tauri, so every native call needs this guard.
  const isTauri = () => typeof (window as any).__TAURI_INTERNALS__ !== 'undefined'

  async function chooseWorkspace() {
    if (!isTauri() || wsBusy) return
    wsBusy = true
    try {
      const picked = (await invoke('pick_workspace')) as string | null
      if (picked) {
        await invoke('set_workspace', { path: picked })
      }
    } catch (e) {
      pageLog(`set_workspace failed: ${String(e)}`)
    } finally {
      wsBusy = false
    }
  }

  function onPaste(e: ClipboardEvent) {
    const files = Array.from(e.clipboardData?.files ?? [])
    if (!files.length) return
    e.preventDefault()
    void addFiles(files)
  }

  function fmtBytes(n: number) {
    if (n < 1024) return `${n} B`
    if (n < 1024 * 1024) return `${(n / 1024).toFixed(0)} kB`
    return `${(n / (1024 * 1024)).toFixed(1)} MB`
  }

  function readAsBase64(f: File): Promise<string> {
    return new Promise((resolve, reject) => {
      const r = new FileReader()
      r.onload = () => {
        const s = String(r.result ?? '')
        // data:image/png;base64,.... -> strip through the comma
        resolve(s.slice(s.indexOf(',') + 1))
      }
      r.onerror = () => reject(r.error)
      r.readAsDataURL(f)
    })
  }

  async function pickFiles(e: Event) {
    const input = e.target as HTMLInputElement
    const files = Array.from(input.files ?? [])
    input.value = ''
    await addFiles(files)
  }

  async function addFiles(files: File[]) {
    if (!files.length) return
    attachErr = ''
    for (const f of files) {
      if (!OK_MIME.includes(f.type)) {
        attachErr = `${f.name}: ${f.type || 'unknown type'} is not a supported image`
        continue
      }
      if (f.size > MAX_ATTACH_BYTES) {
        attachErr = `${f.name}: ${fmtBytes(f.size)} exceeds ${fmtBytes(MAX_ATTACH_BYTES)}`
        continue
      }
      if (pending.length >= MAX_ATTACH) {
        attachErr = `at most ${MAX_ATTACH} images per message`
        break
      }
      try {
        const b64 = await readAsBase64(f)
        pending = [
          ...pending,
          { name: f.name, mime: f.type, bytes: f.size, b64, url: URL.createObjectURL(f) },
        ]
      } catch (err) {
        attachErr = `${f.name}: could not read (${err})`
      }
    }
  }

  function dropAttachment(i: number) {
    const [gone] = pending.splice(i, 1)
    if (gone) URL.revokeObjectURL(gone.url)
    pending = [...pending]
  }

  function clearAttachments() {
    for (const a of pending) URL.revokeObjectURL(a.url)
    pending = []
    attachErr = ''
  }

  // Wait for a turn to finish. `busy` cannot be the condition: start_turn only
  // submits, and the host sets busy=true on its own task, so a caller that
  // checks busy immediately sees false and proceeds before anything happened.
  // HostState.turns is monotonic and incremented after persistence, so it is
  // the honest signal.
  async function waitFor(
    what: string,
    pred: () => boolean,
    budgetMs = 15000,
  ): Promise<boolean> {
    const dl = performance.now() + budgetMs
    while (performance.now() < dl) {
      if (pred()) return true
      await new Promise((r) => setTimeout(r, 100))
    }
    pageLog(`selftest: timed out waiting for ${what}`)
    return false
  }

  async function waitTurnDone(prevTurns: number, budgetMs = 90000): Promise<boolean> {
    const dl = performance.now() + budgetMs
    while (performance.now() < dl) {
      const st = await invoke<HostState>('session_state')
      if (st.turns > prevTurns) return true
      await new Promise((r) => setTimeout(r, 250))
    }
    return false
  }

  async function turnCount(): Promise<number> {
    return (await invoke<HostState>('session_state')).turns
  }

  // ---- actions ------------------------------------------------------------
  async function send() {
    const text = draft.trim()
    if ((!text && pending.length === 0) || busy) return
    const atts = pending.map((a) => ({ mime: a.mime, data_b64: a.b64, name: a.name }))
    draft = ''
    // The transcript shows what was attached by name and size, not a thumbnail:
    // history pages carry metadata only, so a thumbnail here would be a preview
    // that disappears the moment the session is reopened.
    const shown = pending.map((a) => ({ mime: a.mime, name: a.name, bytes: a.bytes }))
    transcript?.addUser(text, shown)
    sentDraft = { text, atts: shown }
    turnSessionId = viewId
    liveBuf = []
    clearAttachments()
    try {
      await invoke('start_turn', { text, attachments: atts })
      pageLog(`turn sent with ${atts.length} attachment(s)`)
    } catch (e) {
      pageLog(`start_turn failed: ${e}`)
    }
  }

  async function answer(a: ApprovalAnswer) {
    if (!approval) return
    const id = approval.id
    approval = null
    if (tick !== undefined) {
      window.clearInterval(tick)
      tick = undefined
    }
    try {
      await invoke('respond_approval', { id, answer: a })
    } catch (e) {
      pageLog(`respond_approval failed: ${e}`)
    }
  }

  async function refreshState() {
    try {
      hostState = await invoke<HostState>('session_state')
      if (hostState) {
        mode = hostState.mode
        thinking = hostState.reasoning
        model = hostState.model
      }
    } catch (e) {
      ipc = { ok: false, msg: String(e).slice(0, 140) }
    }
  }

  async function loadSessions() {
    try {
      sessions = await invoke<Session[]>('list_sessions')
      // Order matters: record what the open session looks like, then decide who
      // is unread relative to that. Reversing it marks the session you are
      // looking at as unread.
      recordSeen(sessions)
      applyUnread(sessions)
    } catch (e) {
      pageLog(`list_sessions failed: ${e}`)
    }
  }

  async function runSearch() {
    const q = query.trim()
    closeDrawerOnMobile()
    if (!q) {
      hits = []
      searchErr = ''
      return
    }
    try {
      hits = await invoke<SearchHit[]>('search_messages', { query: q })
      searchErr = ''
    } catch (e) {
      hits = []
      searchErr = String(e).slice(0, 120)
    }
  }

  function closeDrawerOnMobile() {
    // The drawer overlays the transcript on narrow screens; leaving it open after
    // a tap hides the thing the tap was meant to show.
    if (matchMedia('(max-width: 767px)').matches) sidebarOpen = false
  }

  // The view is the front end's business: the transcript comes straight from
  // the store, so browsing during a turn does not wait on a host command that
  // is parked behind that turn. The host is still told, so the *next* message
  // goes to the session being viewed.
  async function openSession(id: number) {
    closeDrawerOnMobile()
    if (id === viewId) return
    await resyncView(id)
    try {
      await invoke('switch_session', { id })
    } catch (e) {
      pageLog(`switch_session failed: ${e}`)
    }
  }

  async function resyncView(id: number) {
    viewId = id
    transcript?.clear()
    try {
      await loadTranscript(id)
    } catch (e) {
      pageLog(`session_messages failed: ${e}`)
    }
    // Returning to the session that owns a running turn: the stored history
    // does not contain that turn yet, so replay what we buffered.
    if (turnSessionId === id && sentDraft) {
      transcript?.addUser(sentDraft.text, sentDraft.atts)
      for (const e of liveBuf) transcript?.apply(e)
    }
    await refreshState()
  }

  async function newSession() {
    let id: number | null = null
    try {
      id = await invoke<number>('new_session', { title: 'new session' })
    } catch (e) {
      pageLog(`new_session failed: ${e}`)
      return
    }
    if (id !== null) await resyncView(id)
    try {
      await invoke('switch_session', { id })
    } catch (e) {
      pageLog(`switch_session failed: ${e}`)
    }
    await loadSessions()
  }

  function startRename(id: number, current: string) {
    editingId = id
    editTitle = current
  }

  async function commitRename() {
    if (editingId == null) return
    const id = editingId
    editingId = null
    const title = editTitle.trim()
    editTitle = ''
    if (!title) return
    try {
      await invoke('rename_session', { id, title })
      await loadSessions()
    } catch (e) {
      pageLog(`rename_session failed: ${e}`)
    }
  }

  async function removeSession(id: number) {
    // Two-step inline confirm rather than window.confirm: WebKitGTK does not
    // reliably present a JS dialog, and a confirm() that silently returns false
    // would make the delete button look broken. Destructive actions need visible
    // intent.
    if (confirmDeleteId !== id) {
      confirmDeleteId = id
      setTimeout(() => {
        if (confirmDeleteId === id) confirmDeleteId = null
      }, 4000)
      return
    }
    confirmDeleteId = null
    try {
      await invoke('delete_session', { id })
      await loadSessions()
    } catch (e) {
      pageLog(`delete_session failed: ${e}`)
    }
  }

  async function setMode(m: string) {
    mode = m as any
    await invoke('set_mode', { mode: m }).catch((e) => pageLog(`set_mode: ${e}`))
  }

  // Two very different reasons end up with an empty list, and showing the same
  // "no models" for both sends the user to the wrong fix: an endpoint that does
  // not implement /models is normal, an endpoint that is not reachable is the
  // whole app being disconnected.
  let modelErr = $state('')

  async function loadModels() {
    try {
      models = await invoke<string[]>('list_models')
      modelErr = ''
    } catch (e) {
      pageLog(`list_models: ${e}`)
      models = []
      modelErr = String(e)
    }
  }

  let hasOlder = $state(false)
  let oldestId = $state<number | null>(null)
  let loadingOlder = $state(false)

  async function loadTranscript(id: number) {
    const page = await invoke<{ messages: any[]; has_older: boolean }>('session_messages', {
      id,
    })
    transcript?.loadHistory(page.messages)
    hasOlder = page.has_older
    oldestId = page.messages.length ? page.messages[0].id : null
  }

  async function loadOlder() {
    if (oldestId === null || loadingOlder) return
    loadingOlder = true
    try {
      const page = await invoke<{ messages: any[]; has_older: boolean }>('session_messages', {
        id: hostState?.session_id ?? 0,
        before: oldestId,
      })
      if (page.messages.length) {
        const { shift } = transcript?.prependHistory(page.messages) ?? { shift: 0 }
        // Prepending pushes the viewport down; anchor it back to where the
        // reader was instead of dumping them at the top of the new page.
        const sc = transcript?.scrollEl()
        if (sc) sc.scrollTop += shift
        oldestId = page.messages[0].id
      }
      hasOlder = page.has_older
    } catch (e) {
      pageLog(`load_older: ${e}`)
    } finally {
      loadingOlder = false
    }
  }

  async function setModel(m: string) {
    model = m
    await invoke('set_model', { model: m }).catch((e) => pageLog(`set_model: ${e}`))
    await refreshState()
  }
  async function setThinking(l: string) {
    thinking = l as any
    await invoke('set_reasoning', { level: l }).catch((e) => pageLog(`set_reasoning: ${e}`))
  }

  /**
   * Dev-only end-to-end check of the session commands. Clicking is not always
   * available (CI, scripted runs), and "the button exists" is not evidence that
   * rename/delete/search round-trip through the store correctly.
   */
  async function selfTest() {
    let fails = 0
    // Counting failures matters: a harness that logs FAIL and then prints "ok"
    // is worse than no harness, because the last line is the one you read.
    const fail = (m: string) => {
      fails++
      pageLog(`SELFTEST FAIL: ${m}`)
    }
    try {
      const before = await invoke<Session[]>('list_sessions')
      await invoke('new_session', { title: 'selftest' })
      await new Promise((r) => setTimeout(r, 400))
      const mid = await invoke<Session[]>('list_sessions')
      if (mid.length !== before.length + 1) fail(`new_session: ${before.length} -> ${mid.length}`)
      const target = mid.find((s) => s.title === 'selftest')
      if (!target) return fail('selftest session not listed')

      await invoke('rename_session', { id: target.id, title: 'selftest-renamed' })
      const renamed = await invoke<Session[]>('list_sessions')
      if (!renamed.some((s) => s.title === 'selftest-renamed')) fail('rename did not stick')

      const empty = await invoke<any>('session_messages', { id: target.id })
      pageLog(`RAW session_messages -> ${JSON.stringify(empty)}`)
      if (empty.messages.length !== 0) fail(`new session should be empty, got ${empty.messages.length}`)
      if (empty.has_older !== false) fail('empty session should not report older messages')
      // Paging arithmetic is covered by the engine's store test; here we only
      // check the command's shape, which is what the UI actually consumes.
      const paged = await invoke<any>('session_messages', { id: target.id, limit: 2 })
      if (!Array.isArray(paged.messages) || typeof paged.has_older !== 'boolean') {
        fail('session_messages did not return { messages, has_older }')
      }

      const emptiesBefore = (await invoke<Session[]>('list_sessions')).filter(
        (s) => s.message_count === 0,
      ).length

      await invoke('delete_session', { id: target.id })
      // Deleting the active session moves the host off the row asynchronously;
      // read after it has had a chance to land, or the counts below are noise.
      await new Promise((r) => setTimeout(r, 400))
      const after = await invoke<Session[]>('list_sessions')
      if (after.some((s) => s.id === target.id)) fail('delete did not remove the row')

      // The replacement session used to be created with an empty title, so the
      // sidebar kept a blank row for a session the user had just deleted.
      const blank = after.filter((s) => !s.title || !s.title.trim())
      if (blank.length) fail(`${blank.length} session(s) have an empty title after delete`)

      // It must also not pile up one empty orphan per delete. Compare against the
      // count before, since a real DB already contains empty sessions.
      const emptiesAfter = after.filter((s) => s.message_count === 0).length
      if (emptiesAfter > emptiesBefore)
        fail(`delete accumulated empty sessions: ${emptiesBefore} -> ${emptiesAfter}`)

      // Renaming to blank must be refused rather than producing an untitled row.
      let rejected = false
      try {
        await invoke('rename_session', { id: after[0]?.id ?? 1, title: '   ' })
      } catch {
        rejected = true
      }
      if (!rejected) fail('blank rename was accepted')

      const hits = await invoke<SearchHit[]>('search_messages', { query: 'AND OR NOT' })

      // Model picker: the endpoint should offer a list, and switching must be
      // observable in session_state rather than only in the DOM.
      const ms = await invoke<string[]>('list_models')
      if (!ms.length) fail('list_models returned nothing')
      const pick = ms[0]
      await invoke('set_model', { model: pick })
      const st = await invoke<HostState>('session_state')
      if (st.model !== pick) fail(`set_model did not stick: state=${st.model} wanted=${pick}`)

      // Re-selecting a session has to actually move the host AND make the UI
      // reload. invoke() resolving only proves the command was accepted.
      const stNow = await invoke<HostState>('session_state')
      const other = after.find((s) => s.id !== stNow.session_id)
      if (other) {
        const evBefore = sessionChangedCount
        const domBefore = (rootEl?.textContent ?? '').trim()
        await openSession(other.id)
        const deadline = performance.now() + 5000
        while (sessionChangedCount === evBefore && performance.now() < deadline) {
          await new Promise((r) => setTimeout(r, 100))
        }
        if (sessionChangedCount === evBefore)
          fail(`openSession(${other.id}) produced no sessionChanged event`)
        const stAfter = await invoke<HostState>('session_state')
        if (stAfter.session_id !== other.id)
          fail(`host did not move: state=${stAfter.session_id} wanted=${other.id}`)
        // Give the event-driven transcript reload a chance to land.
        let domChanged = false
        const d2 = performance.now() + 3000
        while (performance.now() < d2) {
          await new Promise((r) => setTimeout(r, 100))
          if ((rootEl?.textContent ?? '').trim() !== domBefore) {
            domChanged = true
            break
          }
        }
        if (!domChanged && other.message_count !== stNow.turns)
          fail(`transcript DOM unchanged after selecting session ${other.id}`)
      }

      // The session row menu: opens, offers rename and delete, fits in the
      // window, and closes on an outside click.
      {
        const more = document.querySelector('.sessrow .iconbtn.more') as HTMLElement | null
        if (!more) fail('no session row action button rendered')
        else {
          more.click()
          await new Promise((r) => setTimeout(r, 250))
          const pop = document.querySelector('.pop')
          if (!pop) fail('row menu did not open')
          else {
            const items = [...pop.querySelectorAll('.pop-item')].map((i) => i.textContent!.trim())
            if (!items.includes('Rename')) fail(`row menu missing Rename: ${items.join(', ')}`)
            if (!items.some((i) => i.startsWith('Delete')))
              fail(`row menu missing Delete: ${items.join(', ')}`)
            const r = pop.getBoundingClientRect()
            if (r.top < 0 || r.bottom > window.innerHeight)
              fail(`row menu clipped: top=${Math.round(r.top)} bottom=${Math.round(r.bottom)} win=${window.innerHeight}`)
            document
              .querySelector('.scroller')
              ?.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true }))
            await new Promise((r) => setTimeout(r, 200))
            if (document.querySelector('.pop')) fail('row menu did not close on outside click')
          }
        }
      }

      // Links: navigation inside the webview is always prevented, and the URL is
      // handed to the opener plugin. file:// must be refused by the plugin's
      // scope - if it reports "opened", the scope is too wide and that is a bug.
      {
        const st = await linkProbe('file:///etc/passwd')
        if (st === 'inert: no opener') fail('opener plugin not reachable in the desktop app')
        else if (st === 'opened') fail('opener scope accepted a file:// URL')
        else if (!st.startsWith('failed')) fail(`unexpected link probe: ${st}`)
      }

      // Attachments: the tag renders, the payload survives the round trip to the
      // store, and the turn completes. Whether the endpoint actually *sees* the
      // image is a property of the server, so an error event is logged rather
      // than failed on - but a turn that never ends is a bug.
      {
        const PNG1x1 =
          'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNgYGBgAAAABQABpfZFQAAAAABJRU5ErkJggg=='
        transcript?.addUser('attachment probe', [
          { mime: 'image/png', name: 'probe.png', bytes: 70 },
        ])
        const tags = [...document.querySelectorAll('.atttag')]
        if (!tags.length) fail('attachment tag not rendered in the transcript')
        else if (!tags[tags.length - 1].textContent?.includes('probe.png'))
          fail(`attachment tag missing the name: ${tags[tags.length - 1].textContent}`)

        const errs: string[] = []
        const restore = beginEventTap((e) => {
          if (e.kind === 'error') errs.push(e.d.message)
        })
        const turnsBefore = await turnCount()
        await invoke('start_turn', {
          text: 'What colour is this image? Answer in one word.',
          attachments: [{ mime: 'image/png', data_b64: PNG1x1, name: 'probe.png' }],
        })
        const done = await waitTurnDone(turnsBefore)
        restore()
        if (!done) fail('attachment turn never ended')
        for (const e of errs) pageLog(`SELFTEST note: endpoint said: ${e}`)

        const st = await invoke<HostState>('session_state')
        const page = await invoke<any>('session_messages', { id: st.session_id })
        const withAtt = (page.messages ?? []).filter(
          (m: any) => Array.isArray(m.attachments) && m.attachments.length,
        )
        if (!withAtt.length) fail('attachment metadata did not come back from the store')
        else {
          const a = withAtt[0].attachments[0]
          if (a.name !== 'probe.png') fail(`attachment name lost in storage: ${JSON.stringify(a)}`)
          if (a.bytes < 60 || a.bytes > 80) fail(`attachment size wrong: ${a.bytes}`)
        }
      }

      // The approval round-trip, end to end. Every previous "approval verified"
      // check only proved the request arrived; none proved the answer landed. It
      // did not: the pump and the command used two different maps, so every gated
      // tool call failed closed no matter what the user clicked.
      {
        const evs: string[] = []
        const seen: string[] = []
        const probe = (e: EngineEvent) => {
          seen.push(e.kind)
          if (e.kind === 'toolExecEnd') evs.push(`end:${e.d.ok}`)
        }
        const before = sessionChangedCount
        void before
        // Wrap handle for the duration of this step by listening to the raw stream.
        const restore = beginEventTap(probe)
        const approvalTurnsBefore = await turnCount()
        await invoke('set_mode', { mode: 'ask' })
        await invoke('start_turn', { text: 'Use the exec tool to run: echo cowry-ok' })
        const dl = performance.now() + 90000
        while (!approval && performance.now() < dl) {
          await new Promise((r) => setTimeout(r, 200))
        }
        if (!approval) {
          fail('no approval request arrived for the exec call')
        } else {
          await answer('allow')
          const end = performance.now() + 90000
          while (!evs.length && performance.now() < end) {
            await new Promise((r) => setTimeout(r, 200))
          }
          if (!evs.length) fail('tool never finished after allow')
          else if (!evs[0].endsWith('true')) fail(`tool failed after allow: ${evs[0]}`)
        }
        restore()
        // Same trap as above: waiting on `busy` here can pass before the host
        // has even started the turn.
        if (!(await waitTurnDone(approvalTurnsBefore))) fail('approval turn never ended')
      }

      // Workspace switching. The engine is the authority on what a workspace may
      // be, so a bad path must be refused *and* the previous one kept.
      const wsBefore = (await invoke<HostState>('session_state')).workspace
      await invoke('set_workspace', { path: '/nonexistent/definitely-not' })
      await new Promise((r) => setTimeout(r, 500))
      const wsBad = (await invoke<HostState>('session_state')).workspace
      if (wsBad !== wsBefore) fail(`a non-directory workspace was adopted: ${wsBad}`)

      await invoke('set_workspace', { path: '/tmp' })
      await new Promise((r) => setTimeout(r, 500))
      const wsTmp = (await invoke<HostState>('session_state')).workspace
      if (wsTmp !== '/tmp') fail(`workspace switch did not land: ${wsTmp}`)

      await invoke('set_workspace', { path: wsBefore })
      await new Promise((r) => setTimeout(r, 500))
      const wsBack = (await invoke<HostState>('session_state')).workspace
      if (wsBack !== wsBefore) fail(`workspace was not restored: ${wsBack}`)
      // Say it out loud: a silent pass is indistinguishable from a skipped step.
      pageLog(`workspace: rejected bad path, moved to /tmp, restored to ${wsBack}`)

      // --- browsing during a live turn -------------------------------------
      // This is the reported bug: clicking another session while the model was
      // answering appeared to do nothing, and the running turn's thinking block
      // was painted over whatever session was on screen, which read as a
      // conversation that had lost its history.
      const sessA = await invoke<number>('new_session', { title: 'selftest live A' })
      await resyncView(sessA)
      const sessB = await invoke<number>('new_session', { title: 'selftest live B' })
      const turnsBeforeLive = (await invoke<HostState>('session_state')).turns
      // Go through send(), not invoke(): the user message and the turn's owner
      // are recorded there, and replaying a partial turn depends on them.
      draft = 'Count out loud from one to fifteen, one number per line.'
      await send()

      const sawStream = await waitFor('stream to start', () => busy && turnSessionId === sessA, 30000)
      if (!sawStream) fail('turn never started for the live-browsing check')

      if (sawStream) {
        // Switch the view while the turn is still running.
        await resyncView(sessB)
        if (viewId !== sessB) fail(`view did not move during a turn: viewId=${viewId}`)
        if (turnSessionId !== sessA) fail(`turn ownership lost: turnSessionId=${turnSessionId}`)
        const bText = rootEl?.innerText ?? ''
        if (/one|\b1\b/i.test(bText) && !bText.includes('selftest live B')) {
          fail('the running turn was painted into a different session')
        }

        // Come back: the partial answer must be there, not a blank session.
        await resyncView(sessA)
        const backText = rootEl?.innerText ?? ''
        if (!/one|\b1\b/i.test(backText)) {
          fail(`returning to the running session lost its output: ${backText.slice(0, 120)}`)
        }
        if (!(await waitTurnDone(turnsBeforeLive))) fail('live turn never finished')
      }
      await invoke('delete_sessions', { ids: [sessA, sessB] })

      // --- bulk delete ------------------------------------------------------
      const beforeBulk = await invoke<Session[]>('list_sessions')
      const junk: number[] = []
      for (let i = 0; i < 3; i++) junk.push(await invoke<number>('new_session', { title: `selftest junk ${i}` }))
      const midBulk = await invoke<Session[]>('list_sessions')
      if (midBulk.length !== beforeBulk.length + 3) {
        fail(`new_session did not create 3 rows: ${beforeBulk.length} -> ${midBulk.length}`)
      }
      const removed = await invoke<number>('delete_sessions', { ids: junk })
      if (removed !== 3) fail(`delete_sessions removed ${removed}, expected 3`)
      const afterBulk = await invoke<Session[]>('list_sessions')
      if (afterBulk.length !== beforeBulk.length) {
        fail(`bulk delete did not restore the count: ${beforeBulk.length} -> ${afterBulk.length}`)
      }

      pageLog(
        `${fails === 0 ? 'SELFTEST ok' : `SELFTEST FAILED (${fails})`}: ` +
          `sessions ${before.length}->${after.length}, fts-safe query -> ` +
          `${hits.length} hits, models=${ms.length} picked=${pick}`,
      )
    } catch (e) {
      fail(String(e))
      pageLog(`SELFTEST FAILED (${fails}): aborted before the summary`)
    }
  }

  // y / a / n answer an approval without hunting for the mouse. Ignored while a
  // text field has focus, so typing "no" into the composer cannot deny a tool call.
  // App-wide shortcuts. Deliberately modifier-gated: an unmodified key has to
  // reach the composer, and a bare "?" would be typed into a message.
  function onGlobalKey(e: KeyboardEvent) {
    const mod = e.ctrlKey || e.metaKey
    const key = e.key.toLowerCase()

    if (mod && key === 'k') {
      e.preventDefault()
      composerEl?.focus()
      return
    }
    if (mod && key === 'b') {
      e.preventDefault()
      sidebarOpen = !sidebarOpen
      return
    }
    if (mod && key === 'n') {
      e.preventDefault()
      void newSession()
      return
    }
    if (mod && key === 'enter') {
      e.preventDefault()
      void send()
      return
    }
    if (e.altKey && /^[1-9]$/.test(e.key)) {
      const target = sessions[Number(e.key) - 1]
      if (target) {
        e.preventDefault()
        void openSession(target.id)
      }
      return
    }
  }

  function onApprovalKey(e: KeyboardEvent) {
    if (e.key === 'Escape' && menu) {
      closeMenu()
      return
    }
    if (!approval) return
    const t = e.target as HTMLElement | null
    if (t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.isContentEditable)) return
    const k = e.key.toLowerCase()
    if (k === 'y') { e.preventDefault(); answer('allow') }
    else if (k === 'a') { e.preventDefault(); answer('allowSession') }
    else if (k === 'n' || k === 'escape') { e.preventDefault(); answer('deny') }
  }

  // ---- window controls (frameless) ---------------------------------------
  // getCurrentWindow() throws if window.__TAURI_INTERNALS__ is missing. Called at
  // component-init that would blank the entire page with no visible error, so
  // resolve it lazily and record the failure instead.
  let win: Awaited<ReturnType<typeof getCurrentWindow>> | null = null
  let winErr = ''
  try {
    win = getCurrentWindow()
  } catch (e) {
    winErr = String(e)
    console.error('[cowry] getCurrentWindow failed:', e)
  }
  // A missing permission, a missing window handle, or a null `win` all look
  // identical from the outside: the button does nothing. Log which one it was.
  function winFail(what: string, e: unknown) {
    pageLog(`window ${what} failed: ${e}${win ? '' : ' (no window handle)'}`)
  }
  async function closeWin() {
    try {
      await win?.close()
    } catch (e) {
      winFail('close', e)
    }
  }
  async function minimize() {
    try {
      await win?.minimize()
    } catch (e) {
      winFail('minimize', e)
    }
  }
  async function toggleMax() {
    if (!win) {
      winFail('maximize', 'getCurrentWindow() failed')
      return
    }
    try {
      const m = await win.isMaximized()
      if (m) await win.unmaximize()
      else await win.maximize()
    } catch (e) {
      winFail('maximize', e)
    }
  }

  // ---- render self-report ------------------------------------------------
  // M0 lesson: every machine-checkable gate passed while the webview was showing
  // a stale vite error overlay. Process liveness, memory and sockets cannot tell
  // you whether the UI painted, so the UI says so, and Rust logs it.
  async function reportRender(tag = 'mount') {
    // Convention (same as EngineEvent): Tauri command *argument names* are
    // camelCase (`{ report }`), but *struct fields* keep serde defaults, so they
    // are snake_case on the wire.
    const report = {
      titlebar: !!document.querySelector('.titlebar'),
      sidebar: !!document.querySelector('aside'),
      footer: !!document.querySelector('footer'),
      stream_el: !!document.querySelector('.transcript'),
      body_bg: getComputedStyle(document.body).backgroundColor,
      overlay_present: !!document.querySelector('vite-error-overlay'),
      text_len: document.body.innerText.trim().length,
      device_pixel_ratio: window.devicePixelRatio,
    }
    try {
      await invoke('render_report', { report })
      ipc = { ok: true, msg: 'ok' }
    } catch (e) {
      ipc = { ok: false, msg: String(e).slice(0, 140) }
      pageLog(`render_report FAILED: ${String(e)}`)
    }
  }

  // ---- lifecycle -----------------------------------------------------------
  onMount(async () => {
    if (rootEl) transcript = new Transcript(rootEl)
    // Test seam: lets a browser harness drive the transcript without an engine.
    if (import.meta.env.DEV) (window as any).__cowryTranscript = transcript
    await attachChannel()
    // Non-blocking: a missing /models must not delay first paint.
    void loadModels()
    try {
      gpu = await invoke<GpuReport>('gpu_report')
    } catch (e) {
      gpu = null
      ipc = { ok: false, msg: String(e).slice(0, 140) }
      pageLog(`gpu_report FAILED: ${String(e)}`)
    }
    await refreshState()
    await loadSessions()
    if (winErr) pageLog(`getCurrentWindow FAILED: ${winErr}`)
    pageLog(`mounted text=${document.body.innerText.trim().length}`)
    setTimeout(() => void reportRender('mount'), 400)
    setTimeout(() => void reportRender('late'), 2000)

    // Dev harness: ?autoturn (or VITE_AUTOTURN=1) sends a real first message on
    // launch, so the whole engine -> IPC -> DOM path can be driven from a script.
    // VITE_AUTOTURN_PROMPT picks the message, which is how the tool-call and
    // approval paths get exercised without anyone clicking.
    if (location.search.includes('autoturn') || import.meta.env.VITE_AUTOTURN) {
      const prompt = import.meta.env.VITE_AUTOTURN_PROMPT || 'Say hello in five words.'
      setTimeout(() => {
        draft = prompt
        void send()
      }, 1500)
    }
    if (import.meta.env.VITE_SELFTEST) {
      setTimeout(() => void selfTest(), 1200)
    }
  })

  onDestroy(stopFrames)

  // Window-level: the root div has no tabindex, so a keydown bound there would
  // simply never fire.
  function onDocDown(e: Event) {
    if (!menu) return
    const t = e.target as HTMLElement | null
    // Clicks inside the menu belong to the menu; clicks on an opener belong to
    // that button's own toggle, which would otherwise close and immediately
    // reopen it.
    if (t && (t.closest('.pop') || t.closest('[data-menu]'))) return
    closeMenu()
  }

  onMount(() => {
    initTheme()
    loadSeen()
    window.addEventListener('keydown', onApprovalKey)
    window.addEventListener('keydown', onGlobalKey)
    window.addEventListener('pointerdown', onDocDown)
    window.addEventListener('resize', reposition)
    return () => {
      window.removeEventListener('keydown', onApprovalKey)
      window.removeEventListener('keydown', onGlobalKey)
      window.removeEventListener('pointerdown', onDocDown)
      window.removeEventListener('resize', reposition)
    }
  })

  function onKey(e: KeyboardEvent) {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault()
      void send()
    }
  }

  const gpuOk = $derived(!!gpu && gpu.render_nodes.includes('renderD128') && !gpu.leaked_nvidia)
</script>

<div class="flex h-full flex-col bg-paper text-ink">
  <!-- ============================ titlebar ============================ -->
  <div
    class="titlebar flex h-11 shrink-0 items-center gap-2 border-b border-hairline bg-paper-2 px-3"
    data-tauri-drag-region
  >
    <img src="/icon.png" alt="" class="h-5 w-5 rounded" />
    <span class="text-[13px] font-medium tracking-tight">cowry</span>
    <!-- Sidebar toggle: below 768px the sidebar is a drawer, and without this
         button the session list and search are simply unreachable. -->
    <button class="iconbtn sidemenu" onclick={() => (sidebarOpen = !sidebarOpen)} aria-label="Sessions">
      <svg width="14" height="14" viewBox="0 0 14 14"><path d="M2 4h10M2 7h10M2 10h10" stroke="currentColor" stroke-width="1.3"/></svg>
    </button>
    <span class="ml-2 text-[11px] text-ink-3">{model || 'connecting…'}</span>

    <div class="ml-auto flex items-center gap-1">
      <button class="wbtn" onclick={minimize} aria-label="Minimize">
        <svg width="12" height="12" viewBox="0 0 12 12"><path d="M2 6h8" stroke="currentColor" stroke-width="1.3"/></svg>
      </button>
      <button class="wbtn" onclick={toggleMax} aria-label="Maximize">
        <svg width="12" height="12" viewBox="0 0 12 12"><rect x="2.5" y="2.5" width="7" height="7" fill="none" stroke="currentColor" stroke-width="1.3"/></svg>
      </button>
      <button class="wbtn" onclick={closeWin} aria-label="Close">
        <svg width="12" height="12" viewBox="0 0 12 12"><path d="M3 3l6 6M9 3l-6 6" stroke="currentColor" stroke-width="1.3"/></svg>
      </button>
    </div>
  </div>

  <!-- ============================= body ============================== -->
  <div class="flex min-h-0 flex-1">
    {#if sidebarOpen}
      <button class="scrim" onclick={() => (sidebarOpen = false)} aria-label="Close sidebar"></button>
    {/if}
    <aside
      class="sidebar w-[248px] shrink-0 flex-col gap-1.5 border-r border-hairline p-2.5 min-h-0"
      class:open={sidebarOpen}
      style="background: linear-gradient(160deg, var(--color-wash-1), var(--color-wash-2) 55%, var(--color-wash-3));"
    >
      <button class="pill px-2.5 py-1 text-left text-[12.5px] font-medium" onclick={newSession}>
        + New session
      </button>

      <input
        class="pill px-2.5 py-1 text-[12.5px]"
        placeholder="Search messages"
        bind:value={query}
        onkeydown={(e) => e.key === 'Enter' && runSearch()}
      />
      {#if searchErr}<div class="text-[11px] text-danger">{searchErr}</div>{/if}

      <!-- The list scrolls inside the sidebar. Without min-h-0 a flex item's
           min-height defaults to `auto`, i.e. its content height, so N sessions
           grew the sidebar, which grew the row, which pushed the composer and
           the telemetry bar off-screen. -->
      <div class="sesslist min-h-0 flex-1 overflow-y-auto">
      {#if hits.length}
        <div class="mt-2 px-1 text-[11px] uppercase tracking-wide text-ink-3">Search</div>
        {#each hits as h}
          <button class="searchhit" onclick={() => openSession(h.session_id)}>
            <span class="block truncate text-[12.5px]">{h.snippet}</span>
            <span class="block text-[11px] text-ink-3">{h.role} · session {h.session_id}</span>
          </button>
        {/each}
      {:else}
        <div class="sesshead mt-2">
          <span>Sessions</span>
          <button
            class="sesshead-toggle"
            onclick={() => {
              selectMode = !selectMode
              if (!selectMode) selected = new Set()
            }}
          >
            {selectMode ? 'done' : 'select'}
          </button>
        </div>
        {#each sessions as s}
          <div
            class="sessrow {hostState?.session_id === s.id ? 'active' : ''} {unread.has(s.id) ? 'unread' : ''} {selectMode ? 'selecting' : ''}"
          >
            {#if editingId === s.id}
              <input
                class="pill px-2 py-1 text-[12.5px] w-full"
                bind:value={editTitle}
                onkeydown={(e) => {
                  if (e.key === 'Enter') commitRename()
                  else if (e.key === 'Escape') editingId = null
                }}
                onblur={() => commitRename()}
              />
            {:else}
              <!-- One line per session, no message count: the row is a target,
                   not a report. Rename/delete live in a menu so the row stays
                   quiet, and the dot is always laid out (invisible until unread)
                   so titles do not shift when one arrives. -->
              <button
                class="sessbtn"
                onclick={() => (selectMode ? toggleSelect(s.id) : openSession(s.id))}
              >
                {#if selectMode}
                  <span class="sess-check {selected.has(s.id) ? 'on' : ''}" aria-hidden="true"></span>
                {:else}
                  <span class="sess-dot" aria-hidden="true"></span>
                {/if}
                <span class="sess-title">{s.title}</span>
              </button>
              <button
                class="iconbtn more"
                data-menu
                aria-label="More actions"
                onclick={(e) => openMenu('row', e.currentTarget as HTMLElement, s.id)}
              >
                <svg width="12" height="12" viewBox="0 0 12 12"><circle cx="2.6" cy="6" r="1.05" fill="currentColor"/><circle cx="6" cy="6" r="1.05" fill="currentColor"/><circle cx="9.4" cy="6" r="1.05" fill="currentColor"/></svg>
              </button>
            {/if}
          </div>
        {/each}
      {/if}
      </div>
    </aside>

    <!-- main -->
    <main class="flex min-w-0 flex-1 flex-col min-h-0">
      <div class="scroller min-h-0 flex-1 overflow-y-auto px-6 py-6">
        {#if hasOlder}
          <button class="btn-ghost mb-3 text-[12px]" onclick={loadOlder} disabled={loadingOlder}>
            {loadingOlder ? 'loading…' : 'Load older messages'}
          </button>
        {/if}
        <div class="mx-auto w-full max-w-[780px]">
          <div class="transcript" bind:this={rootEl}></div>
        </div>
      </div>

      <!-- approval prompt: it owns the keyboard, so it lives above the composer -->
      {#if approval}
        <div class="approval mx-auto w-full max-w-[780px] px-6">
          <div class="card approval-card">
            <div class="text-[12px] font-semibold uppercase tracking-wide text-warn">
              Approve · {approval.tool} ({approval.risk})
            </div>
            <div class="mt-1 font-mono text-[13px]">{approval.summary}</div>
            <div class="mt-3 flex items-center gap-2">
              <button class="btn-go" onclick={() => answer('allow')}>Allow (y)</button>
              <button class="btn-ghost" onclick={() => answer('allowSession')}>Allow for session (a)</button>
              <button class="btn-bad" onclick={() => answer('deny')}>Deny (n)</button>
              <span class="ml-auto text-[11.5px] text-ink-3" title="Unanswered approvals fail closed">
                auto-denies in {approvalLeft}s
              </span>
            </div>
          </div>
        </div>
      {/if}

      <!-- composer -->
      <div class="px-4 pb-2 pt-2">
        <div class="mx-auto w-full max-w-[860px]">
          <div
            class="cbox"
            role="group"
            aria-label="Message composer"
            class:dropping={dropping}
            onpaste={onPaste}
            ondragover={(e: DragEvent) => {
              if (e.dataTransfer?.types?.includes('Files')) {
                e.preventDefault()
                dropping = true
              }
            }}
            ondragleave={() => (dropping = false)}
            ondrop={(e: DragEvent) => {
              e.preventDefault()
              dropping = false
              void addFiles(Array.from(e.dataTransfer?.files ?? []))
            }}
          >
            {#if pending.length || attachErr}
              <div class="attrow">
                {#each pending as a, i}
                  <span class="attchip">
                    <img src={a.url} alt="" />
                    <span class="att-name">{a.name}</span>
                    <span class="att-size">{fmtBytes(a.bytes)}</span>
                    <button class="att-x" aria-label="Remove attachment" onclick={() => dropAttachment(i)}>×</button>
                  </span>
                {/each}
                {#if attachErr}<span class="att-err">{attachErr}</span>{/if}
              </div>
            {/if}
            <textarea
              class="composer"
              bind:this={composerEl}
              rows="3"
              placeholder="Ask something. Enter to send, Shift+Enter for a newline."
              bind:value={draft}
              onkeydown={onKey}
              disabled={busy}
            ></textarea>

            <div class="crow">
              <!-- The "+" this used to be would duplicate the paperclip now that
                   attachments exist. A disabled button that duplicates a live one
                   is worse than no button. -->
              <input
                type="file"
                accept="image/png,image/jpeg,image/webp,image/gif"
                multiple
                class="attfile"
                bind:this={fileInput}
                onchange={pickFiles}
              />
              <button class="cbtn sq" title="Attach images (png, jpeg, webp, gif)" onclick={() => fileInput?.click()}>
                <svg width="14" height="14" viewBox="0 0 14 14"><path d="M9.2 4.3 5 8.5a1.9 1.9 0 0 0 2.7 2.7l4.2-4.2a3.4 3.4 0 0 0-4.8-4.8L2.8 6.5a4.9 4.9 0 0 0 6.9 6.9" stroke="currentColor" stroke-width="1.2" fill="none"/></svg>
              </button>

              <button class="cbtn chip" data-menu onclick={(e) => openMenu('mode', e.currentTarget as HTMLElement)} aria-expanded={menu?.kind === 'mode'}>
                <svg width="13" height="13" viewBox="0 0 14 14"><circle cx="7" cy="7" r="5.2" fill="none" stroke="currentColor" stroke-width="1.2"/><path d="M5.4 7h3.2M7 5.4v3.2" stroke="currentColor" stroke-width="1.1"/></svg>
                <span>{MODE_LABEL[mode]}</span>
                <svg class="chev" width="10" height="10" viewBox="0 0 10 10"><path d="M2.5 4 5 6.5 7.5 4" stroke="currentColor" stroke-width="1.2" fill="none"/></svg>
              </button>

              <div class="ml-auto flex items-center gap-1">
                <button class="cbtn chip" data-menu onclick={(e) => openMenu('model', e.currentTarget as HTMLElement)} aria-expanded={menu?.kind === 'model'}>
                  <svg width="13" height="13" viewBox="0 0 14 14"><path d="M7.8 1.6 3.4 7.6h2.6l-.8 4.8 4.4-6H7z" fill="currentColor"/></svg>
                  <span class="chip-label">{shortModel(model)}</span>
            {#if modelErr}<span class="chip-warn" title="Endpoint unreachable">!</span>{/if}
                  <svg class="chev" width="10" height="10" viewBox="0 0 10 10"><path d="M2.5 4 5 6.5 7.5 4" stroke="currentColor" stroke-width="1.2" fill="none"/></svg>
                </button>
                <button class="cbtn chip" data-menu onclick={(e) => openMenu('think', e.currentTarget as HTMLElement)} aria-expanded={menu?.kind === 'think'}>
                  <span>{THINK_LABEL[thinking]}</span>
                  <svg class="chev" width="10" height="10" viewBox="0 0 10 10"><path d="M2.5 4 5 6.5 7.5 4" stroke="currentColor" stroke-width="1.2" fill="none"/></svg>
                </button>
                <button class="cbtn sq" disabled title="Dictation is not implemented yet">
                  <svg width="14" height="14" viewBox="0 0 14 14"><rect x="5.6" y="1.8" width="2.8" height="5.6" rx="1.4" fill="none" stroke="currentColor" stroke-width="1.2"/><path d="M3.6 6.6a3.4 3.4 0 0 0 6.8 0M7 10v2.2" stroke="currentColor" stroke-width="1.2" fill="none"/></svg>
                </button>

                <!-- One control for send and stop. A separate "Cancel" text
                     button sat far from the action and, until the host cancel
                     flag was fixed, did nothing. -->
                <button
                  class="cbtn go {approval ? 'warn' : ''}"
                  onclick={() => (busy ? stopTurn() : send())}
                  disabled={!busy && !draft.trim()}
                  title={busy ? 'Stop this turn' : approval ? 'Waiting on an approval above' : 'Send'}
                  aria-label={busy ? 'Stop' : 'Send'}
                >
                  {#if busy}
                    <svg width="14" height="14" viewBox="0 0 14 14"><rect x="4" y="4" width="6" height="6" rx="1.4" fill="currentColor"/></svg>
                  {:else}
                    <svg width="14" height="14" viewBox="0 0 14 14"><path d="M7 11V3M3.6 6.4 7 3l3.4 3.4" stroke="currentColor" stroke-width="1.5" fill="none"/></svg>
                  {/if}
                </button>
              </div>
            </div>

          </div>

          <button
            class="wsrow"
            title="Choose the directory the tools operate in"
            onclick={chooseWorkspace}
          >
            <svg width="13" height="13" viewBox="0 0 14 14"><path d="M1.8 4.2h3.4l1 1.4h6v5.6a.8.8 0 0 1-.8.8H2.6a.8.8 0 0 1-.8-.8z" fill="none" stroke="currentColor" stroke-width="1.1"/></svg>
            <span>{shortWorkspace || 'Choose workspace'}</span>
            {#if wsBusy}<span class="ws-note">opening…</span>{/if}
          </button>
        </div>
      </div>
    </main>
  </div>

  <!-- One menu for everything, positioned fixed. A popover inside .cbox or a
       sidebar row is clipped by that ancestor's overflow and by the window edge.
       Fixed positioning escapes the ancestors, and the coordinates are clamped
       to the viewport. A webview still cannot paint outside its own window, so
       placement is chosen to fit rather than allowed to overflow. -->
  {#if menu}
    <div
      class="pop {menuPos ? '' : 'measuring'}"
      style={menuPos ? `left:${menuPos.x}px; top:${menuPos.y}px` : 'left:0px; top:0px'}
      role="menu"
      bind:this={menuEl}
    >
      {#if menu.kind === 'mode'}
        <div class="pop-title">Approvals</div>
        {#each MODES as m}
          <button class="pop-item {mode === m ? 'on' : ''}" onclick={() => pickMode(m)} role="menuitemradio" aria-checked={mode === m}>
            <span class="pop-lbl">{MODE_LABEL[m]}</span>
            <span class="pop-sub">{MODE_HINT[m]}</span>
          </button>
        {/each}
      {:else if menu.kind === 'think'}
        <div class="pop-title">Thinking effort</div>
        {#each THINKS as t}
          <button class="pop-item {thinking === t ? 'on' : ''}" onclick={() => pickThinking(t)} role="menuitemradio" aria-checked={thinking === t}>
            <span class="pop-lbl">{THINK_LABEL[t]}</span>
          </button>
        {/each}
      {:else if menu.kind === 'model'}
        <div class="pop-title">Model</div>
        {#if models.length}
          {#each models as m}
            <button class="pop-item {model === m ? 'on' : ''}" onclick={() => pickModel(m)} role="menuitemradio" aria-checked={model === m}>
              <span class="pop-lbl pop-mono">{m}</span>
            </button>
          {/each}
        {:else}
          <div class="pop-empty">
            {#if modelErr}
              <strong>Cannot reach the endpoint.</strong><br />
              <span class="pop-mono">{modelErr}</span><br />
              Start the server, or fix the URL in
              <span class="pop-mono">~/.config/cowry/config.toml</span>.
            {:else}
              This endpoint does not list models.<br />
              Using <span class="pop-mono">{model || '\u2014'}</span>
            {/if}
          </div>
        {/if}
      {:else if menu.kind === 'keys'}
        <div class="pop-title">Keyboard</div>
        {#each SHORTCUTS as sc}
          <div class="pop-item static">
            <span class="pop-lbl">{sc.label}</span>
            <span class="pop-key">{sc.keys}</span>
          </div>
        {/each}
      {:else}
        {@const sid = menu.sessionId ?? -1}
        <div class="pop-title">{titleOf(sid)}</div>
        <button class="pop-item" onclick={() => renameFromMenu(sid)} role="menuitem">Rename</button>
        {#if confirmDeleteId === sid}
          <div class="pop-sub pad">Delete this session? Its messages go with it.</div>
          <button class="pop-item danger" onclick={() => removeFromMenu(sid)} role="menuitem">Delete</button>
          <button class="pop-item" onclick={() => (confirmDeleteId = null)} role="menuitem">Cancel</button>
        {:else}
          <button class="pop-item danger" onclick={() => (confirmDeleteId = sid)} role="menuitem">Delete</button>
        {/if}
      {/if}
    </div>
  {/if}

  <!-- ========================== telemetry bar ========================= -->
  <footer class="telem flex h-9 shrink-0 items-center gap-4 border-t border-hairline bg-paper-2 px-4 text-[11.5px] text-ink-2">
    <span title="prefix cache hit — the number that explains most perceived latency on a local model">
      ⚡ cache <b class="text-ink">{fmt.pct(cacheHit)}</b>
      <span class="text-ink-3">({cachedTokens}/{promptTokens})</span>
    </span>
    <span title="effective = prompt_tokens / ttft; raw = server-reported">
      prefill <b class="text-ink">{fmt.rate(curPrefill)}</b>
      <span class="text-ink-3">eff · avg {fmt.rate(avgPrefill)} raw</span>
    </span>
    <span title="live arrival rate vs session average">
      decode <b class="text-ink">{fmt.rate(curDecode)}</b>
      <span class="text-ink-3">· avg {fmt.rate(avgDecode)}</span>
    </span>
    <span class="telem-extra">spec <b class="text-ink">{fmt.pct(specPct)}</b></span>
    <span class="telem-extra">ttft <b class="text-ink">{ttft}</b>ms</span>
    {#if hostState?.context_tokens}
      <span>ctx <b class="text-ink">{hostState.context_tokens}</b></span>
    {/if}
    {#if hostState?.compactions}
      <span class="text-note" title="history was dropped; the next prefill will be slow">
        compacted {hostState.compactions}
      </span>
    {/if}

    <span class="ml-auto flex items-center gap-3">
      <span
        class="rounded-full px-2 py-0.5 {ipc.ok ? 'bg-paper-2 text-ink-3' : 'bg-danger-bg text-danger'}"
        title="Tauri IPC round-trip: {ipc.msg}"
      >
        ipc {ipc.ok ? '✓' : '✗'}
      </span>
      <span class="telem-extra" title="frames sampled during the last turn">
        jank <b class={dropped > 0 ? 'text-shell-deep' : 'text-ink'}>{dropped}</b>
        <span class="text-ink-3">/ {frames} f · worst {worstFrame.toFixed(0)}ms</span>
      </span>
      <span
        class="rounded-full px-2 py-0.5 {gpuOk ? 'bg-ok-bg text-ok' : 'bg-danger-bg text-danger'}"
        title={gpu ? `EGL: ${gpu.egl_vendor}\nnodes: ${gpu.render_nodes.join(', ')}` : 'gpu_report unavailable'}
      >
        {gpuOk ? 'iGPU renderD128' : gpu ? `GPU LEAK: ${gpu.render_nodes.join(',')}` : 'gpu ?'}
      </span>
      <button
        class="keyhelp"
        title="Toggle light / dark"
        aria-label="Toggle light or dark theme"
        onclick={() => applyTheme(theme === 'dark' ? 'light' : 'dark')}
      >
        {theme === 'dark' ? '\u2600' : '\u263e'}
      </button>
      <button
        class="keyhelp"
        data-menu
        title="Keyboard shortcuts"
        aria-label="Keyboard shortcuts"
        onclick={(e) => openMenu('keys', e.currentTarget as HTMLElement)}
      >
        ?
      </button>
    </span>
  </footer>
</div>
