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
  import { Transcript } from './lib/transcript'
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
  let editTitle = $state('')
  let confirmDeleteId = $state<number | null>(null)
  let sidebarOpen = $state(false)
  let models = $state<string[]>([])

  // Custom popovers instead of <select>: native dropdowns render with the
  // toolkit's own chrome, which cannot be themed, and they look nothing like the
  // rest of the app.
  type MenuKind = 'mode' | 'model' | 'think' | 'row'
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
  let eventTaps: ((e: EngineEvent) => void)[] = []
  function beginEventTap(fn: (e: EngineEvent) => void) {
    eventTaps.push(fn)
    return () => {
      eventTaps = eventTaps.filter((t) => t !== fn)
    }
  }

  function handle(ev: EngineEvent) {
    for (const t of eventTaps) t(ev)
    transcript?.apply(ev)
    switch (ev.kind) {
      case 'sessionChanged':
        sessionChangedCount++
        // The host tells us when the switch actually landed. Loading the
        // transcript from the UI right after invoke() would race the host task
        // and can render the previous session's messages under the new name.
        void (async () => {
          try {
            await loadTranscript(ev.d.session_id)
          } catch (e) {
            pageLog(`session_messages after switch failed: ${e}`)
          }
          await refreshState()
          await loadSessions()
        })()
        break
      case 'turnStart':
        model = ev.d.model
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
        transcript?.flushNow()
        busy = false
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

  // ---- actions ------------------------------------------------------------
  async function send() {
    const text = draft.trim()
    if (!text || busy) return
    draft = ''
    transcript?.addUser(text)
    try {
      await invoke('start_turn', { text })
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

  async function openSession(id: number) {
    // Fire the command; the transcript reload happens on the sessionChanged
    // event so it is ordered after the host has actually moved.
    closeDrawerOnMobile()
    try {
      await invoke('switch_session', { id })
    } catch (e) {
      pageLog(`switch_session failed: ${e}`)
    }
  }

  async function newSession() {
    await invoke('new_session', { title: 'new session' })
    transcript?.clear()
    await refreshState()
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

  async function loadModels() {
    try {
      models = await invoke<string[]>('list_models')
    } catch (e) {
      // An endpoint without /models is a normal configuration. Fall back to the
      // configured id rather than showing an error the user cannot act on.
      pageLog(`list_models: ${e}`)
      models = []
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
        // A real deadline: `performance.now() < performance.now() + N` is always
        // true, which would spin here forever if the turn never ended.
        const quiet = performance.now() + 60000
        while (busy && performance.now() < quiet) {
          await new Promise((r) => setTimeout(r, 300))
        }
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
    loadSeen()
    window.addEventListener('keydown', onApprovalKey)
    window.addEventListener('pointerdown', onDocDown)
    window.addEventListener('resize', reposition)
    return () => {
      window.removeEventListener('keydown', onApprovalKey)
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
      {#if searchErr}<div class="text-[11px] text-[#8b3d3d]">{searchErr}</div>{/if}

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
        <div class="mt-2 px-1 text-[11px] uppercase tracking-wide text-ink-3">Sessions</div>
        {#each sessions as s}
          <div
            class="sessrow {hostState?.session_id === s.id ? 'active' : ''} {unread.has(s.id) ? 'unread' : ''}"
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
              <button class="sessbtn" onclick={() => openSession(s.id)}>
                <span class="sess-dot" aria-hidden="true"></span>
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
            <div class="text-[12px] font-semibold uppercase tracking-wide text-[#8a6d1f]">
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
          <div class="cbox">
            <textarea
              class="composer"
              rows="3"
              placeholder="Ask something. Enter to send, Shift+Enter for a newline."
              bind:value={draft}
              onkeydown={onKey}
              disabled={busy}
            ></textarea>

            <div class="crow">
              <!-- Placeholders for features that do not exist yet. They are
                   disabled rather than absent so the layout matches where the
                   control will land, and the title says why it is inert. -->
              <button class="cbtn sq" disabled title="Not implemented yet">
                <svg width="14" height="14" viewBox="0 0 14 14"><path d="M7 2.5v9M2.5 7h9" stroke="currentColor" stroke-width="1.4"/></svg>
              </button>
              <button class="cbtn sq" disabled title="Attachments are not implemented yet">
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

          <button class="wsrow" disabled title="Workspace picking is not implemented yet">
            <svg width="13" height="13" viewBox="0 0 14 14"><path d="M1.8 4.2h3.4l1 1.4h6v5.6a.8.8 0 0 1-.8.8H2.6a.8.8 0 0 1-.8-.8z" fill="none" stroke="currentColor" stroke-width="1.1"/></svg>
            <span>Choose workspace (optional)</span>
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
          <div class="pop-empty">This endpoint does not list models.<br />Using <span class="pop-mono">{model || '\u2014'}</span></div>
        {/if}
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
      <span class="text-[#7a4fa0]" title="history was dropped; the next prefill will be slow">
        compacted {hostState.compactions}
      </span>
    {/if}

    <span class="ml-auto flex items-center gap-3">
      <span
        class="rounded-full px-2 py-0.5 {ipc.ok ? 'bg-[#eef0e8] text-ink-3' : 'bg-[#f6e6e6] text-[#8b3d3d]'}"
        title="Tauri IPC round-trip: {ipc.msg}"
      >
        ipc {ipc.ok ? '✓' : '✗'}
      </span>
      <span class="telem-extra" title="frames sampled during the last turn">
        jank <b class={dropped > 0 ? 'text-shell-deep' : 'text-ink'}>{dropped}</b>
        <span class="text-ink-3">/ {frames} f · worst {worstFrame.toFixed(0)}ms</span>
      </span>
      <span
        class="rounded-full px-2 py-0.5 {gpuOk ? 'bg-[#e6f0e6] text-[#3d6b3d]' : 'bg-[#f6e6e6] text-[#8b3d3d]'}"
        title={gpu ? `EGL: ${gpu.egl_vendor}\nnodes: ${gpu.render_nodes.join(', ')}` : 'gpu_report unavailable'}
      >
        {gpuOk ? 'iGPU renderD128' : gpu ? `GPU LEAK: ${gpu.render_nodes.join(',')}` : 'gpu ?'}
      </span>
    </span>
  </footer>
</div>
