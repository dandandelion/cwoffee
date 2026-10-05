<script lang="ts">
  import { onMount, onDestroy } from 'svelte'
  import { invoke, Channel, transportKind, gatewayRole, onReconnect, onResync } from './lib/transport'
  import { getCurrentWindow } from '@tauri-apps/api/window'
  import type {
    ApprovalAnswer,
    EngineEvent,
    GpuReport,
    HostState,
    ModelInfo,
    SearchHit,
    Session,
    StoredMessage,
    WorkspaceRow,
  } from './lib/events'
  import { Rolling, statsFromTurn, fmt } from './lib/live'
  import { Transcript, linkProbe, subjectFor } from './lib/transcript'
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
  /**
   * Folders the sidebar offers. The store owns this list: it is the set of
   * folders that were added unioned with the set that hold sessions, so a
   * folder you never added but have work in still shows, and an empty one you
   * did add stays.
   */
  let workspaces = $state<WorkspaceRow[]>([])
  /** Which groups are folded up. Per-client, not on the host: it says how much
   * of your own screen you want to spend on folders. */
  let wsCollapsed = $state<Set<string>>(loadWsCollapsed())
  /** The folder a not-yet-created session will use. Set by a workspace's `+`. */
  let draftWorkspace = $state('')
  let wsAdding = $state(false)
  let wsAddDraft = $state('')
  let wsAddErr = $state('')
  let wsRenaming = $state('')
  let wsLabelDraft = $state('')
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
  // Draft state: no session row exists until the first message is sent, so the
  // sidebar never fills with blanks and "new session" costs nothing.
  let viewMsgCount = $state(0)
  const greeting = pickGreeting()
  let confirmDeleteId = $state<number | null>(null)
  let sidebarOpen = $state(false)
  // Layer promotion is scoped to the animation itself. Held permanently, the
  // compositor drops the layer when the transition ends and re-rasterises the
  // drawer once - a visible flash at the end of open *and* close.
  let sidebarAnimating = $state(false)
  let sidebarAnimTimer: ReturnType<typeof setTimeout> | undefined
  function setSidebar(open: boolean) {
    if (sidebarOpen === open) return
    sidebarOpen = open
    sidebarAnimating = true
    clearTimeout(sidebarAnimTimer)
    sidebarAnimTimer = setTimeout(() => (sidebarAnimating = false), 320)
  }
  let models = $state<ModelInfo[]>([])

  // Custom popovers instead of <select>: native dropdowns render with the
  // toolkit's own chrome, which cannot be themed, and they look nothing like the
  // rest of the app.
  type MenuKind = 'mode' | 'model' | 'think' | 'row' | 'keys' | 'metrics' | 'gateway' | 'ws'

  /** Mirror of the desktop's GatewayStatus. Not reachable over the gateway
      itself: the payload carries the token inside ready-to-open URLs. */
  interface GatewayStatus {
    running: boolean
    host: string
    port: number
    /** Devices, not connections: two tabs on one phone is one device. */
    clients: number
    connections: number
    peers: string[]
    lan: boolean
    has_token: boolean
    token_source: string
    urls: string[]
    tls: boolean
    note: string
  }
  // Anchored in viewport coordinates rather than to the opener, so no ancestor
  // overflow can clip it. `place` records which side it was put on.
  type Menu = {
    kind: MenuKind
    sessionId?: number
    /** Which folder a 'ws' menu is about. */
    wsPath?: string
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

  const WS_KEY = 'cwoffee.wsCollapsed.v1'

  function loadWsCollapsed(): Set<string> {
    try {
      const raw = localStorage.getItem(WS_KEY)
      return new Set<string>(raw ? (JSON.parse(raw) as string[]) : [])
    } catch {
      return new Set()
    }
  }

  function saveWsCollapsed() {
    try {
      localStorage.setItem(WS_KEY, JSON.stringify([...wsCollapsed]))
    } catch {
      /* private mode: a lost fold preference is not worth an error banner */
    }
  }

  function basenameOf(p: string): string {
    const parts = p.replace(/\/+$/, '').split('/')
    return parts[parts.length - 1] || p
  }

  function wsLabelOf(w: { label: string; path: string }): string {
    // An empty label means "show the folder name", which is the store's
    // convention and what people want nine times out of ten.
    return w.label || basenameOf(w.path)
  }

  type WsGroup = { path: string; label: string; list: Session[] }

  /// `new_session` reserves a row before anything has been said in it, so that a
  /// `+` click has somewhere to write the first message. A row like that is not a
  /// conversation, and showing it turned "the session is created lazily" back into
  /// a sidebar full of "new session" clutter as soon as anyone pressed `+`,
  /// Cmd-N, or a harness. It becomes visible when it gets its first message.
  function isBareDraft(s: Session) {
    return s.message_count === 0 && /^new session$/i.test(s.title)
  }

  const wsGroups = $derived.by((): WsGroup[] => {
    const byPath = new Map<string, Session[]>()
    for (const s of sessions) {
      if (isBareDraft(s)) continue
      const k = s.workspace ?? ''
      const arr = byPath.get(k) ?? []
      arr.push(s)
      byPath.set(k, arr)
    }
    const out: WsGroup[] = []
    const seen = new Set<string>()
    for (const w of workspaces) {
      out.push({ path: w.path, label: wsLabelOf(w), list: byPath.get(w.path) ?? [] })
      seen.add(w.path)
    }
    // A session in a folder that is not on the list still has to be reachable.
    // Hiding work because the sidebar had no row for it is the failure mode.
    for (const [path, list] of byPath) {
      if (seen.has(path)) continue
      out.push({ path, label: path ? basenameOf(path) : 'No workspace', list })
      seen.add(path)
    }
    return out
  })

  const draftWsLabel = $derived(
    draftWorkspace
      ? wsLabelOf(workspaces.find((w) => w.path === draftWorkspace) ?? {
          label: '',
          path: draftWorkspace,
        })
      : '',
  )

  function isOpen(path: string) {
    return !wsCollapsed.has(path)
  }

  function expandWs(path: string) {
    const next = new Set(wsCollapsed)
    next.delete(path)
    wsCollapsed = next
    saveWsCollapsed()
  }

  function toggleWs(path: string) {
    if (wsCollapsed.has(path)) expandWs(path)
    else {
      wsCollapsed = new Set([...wsCollapsed, path])
      saveWsCollapsed()
    }
  }

  async function loadWorkspaces() {
    try {
      workspaces = await invoke<WorkspaceRow[]>('list_workspaces')
    } catch (e) {
      // The grouping still works from the sessions themselves; this list only
      // adds the empty folders nobody has written to yet.
      workspaces = []
      pageLog(`list_workspaces failed: ${cleanErr(e)}`)
    }
  }

  /**
   * `+` on a workspace row. No session row is created: the greeting opens with
   * the folder preselected, and the session comes into existence when the first
   * message is sent. Creating a row on click is how sidebars fill up with
   * "new session" rows that were never used.
   */
  function newSessionIn(path: string) {
    newSession()
    draftWorkspace = path
    expandWs(path)
  }

  async function addFolderFor(path: string) {
    wsAddErr = ''
    try {
      const added = await invoke<string>('add_workspace', { path })
      await loadWorkspaces()
      expandWs(added)
    } catch (e) {
      wsAddErr = cleanErr(e)
      pageLog(`add_workspace failed: ${String(e)}`)
    }
  }

  async function addFolderTyped() {
    const path = wsAddDraft.trim()
    if (!path) return
    await addFolderFor(path)
    if (!wsAddErr) {
      wsAddDraft = ''
      wsAdding = false
    }
  }

  /**
   * Desktop only. Picking is deliberately separate from applying: adding a
   * folder to the list must not move the tools out from under the session you
   * are standing in.
   */
  async function addFolderPicked() {
    wsAddErr = ''
    try {
      const picked = (await invoke('pick_workspace')) as string | null
      if (picked) await addFolderFor(picked)
    } catch (e) {
      wsAddErr = cleanErr(e)
      pageLog(`pick_workspace for add failed: ${String(e)}`)
    }
  }

  async function removeFolder(path: string) {
    menu = null
    wsAddErr = ''
    try {
      await invoke('remove_workspace', { path })
      await loadWorkspaces()
    } catch (e) {
      wsAddErr = cleanErr(e)
    }
  }

  function startWsRename(path: string, current: string) {
    menu = null
    wsRenaming = path
    wsLabelDraft = current
  }

  async function commitWsRename() {
    if (!wsRenaming) return
    const path = wsRenaming
    wsRenaming = ''
    try {
      await invoke('set_workspace_label', { path, label: wsLabelDraft })
      await loadWorkspaces()
    } catch (e) {
      wsAddErr = cleanErr(e)
    }
  }

  function openMenu(
    kind: MenuKind,
    anchor: HTMLElement,
    sessionId?: number,
    wsPath?: string,
  ) {
    if (menu && menu.kind === kind && menu.sessionId === sessionId && menu.wsPath === wsPath) {
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
      wsPath,
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

  const SEEN_KEY = 'cwoffee.seenCounts.v1'

  /**
   * Read a stored value under its current key, falling back to the pre-rebrand
   * `cowry.` name and re-saving under the new one. Without this the rebrand
   * silently resets everyone's theme and unread state.
   */
  function readStored(key: string): string | null {
    try {
      const v = localStorage.getItem(key)
      if (v !== null) return v
      const legacy = key.replace(/^cwoffee\./, 'cowry.')
      const old = localStorage.getItem(legacy)
      if (old !== null) localStorage.setItem(key, old)
      return old
    } catch {
      return null
    }
  }

  function loadSeen() {
    try {
      const raw = readStored(SEEN_KEY)
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

  // Time of day for the greeting; the sub-line is seeded by the date, so it
  // changes across days but stays stable within one (a line that changed on
  // every render would be noise, not warmth).
  function pickGreeting() {
    const h = new Date().getHours()
    const hi =
      h < 5 ? 'Still up?' : h < 12 ? 'Good morning.' : h < 18 ? 'Good afternoon.' : 'Good evening.'
    const subs = [
      'Files, spreadsheets, documents, sandboxed commands — on a model you run yourself.',
      'Ask something, or point me at a directory to work in.',
      'I can read, write, run and look things up. Nothing leaves this machine.',
      'Give me a mess. I can turn it into a spreadsheet.',
      'Start typing. The session only exists once you do.',
    ]
    const day = Math.floor(Date.now() / 86_400_000)
    return { hi, sub: subs[day % subs.length] }
  }

  function shortModel(m: string) {
    if (!m) return 'no model'
    return m.length > 22 ? m.slice(0, 21) + '\u2026' : m
  }

  /** 262144 -> "262k". The picker shows several of these side by side. */
  function fmtTokens(n: number) {
    if (n >= 1_000_000) return `${Math.round(n / 1_000_000)}M`
    if (n >= 1000) return `${Math.round(n / 1000)}k`
    return `${n}`
  }

  /** null means the endpoint never said, which is not the same as "no". */
  function visionOf(m: ModelInfo): boolean | null {
    if (!m.input_modalities?.length) return null
    return m.input_modalities.includes('image')
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
  // Why the transport failed, in words the server chose. A rejected token and an
  // unreachable endpoint need different fixes and the UI used to report both as
  // "no model".
  let connNote = $state('')

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
  // True while the decode figure is a character-derived estimate rather than the
  // server's own count. Labelling it is the difference between a live bar and a
  // lying one.
  let liveEstimate = $state(false)
  let thinkingLive = $state(false)

  // ---- jank meter ---------------------------------------------------------
  // Kept from M0: it is the only number that tells us the streaming path is
  // still cheap after the UI got real content.
  let frames = $state(0)
  let dropped = $state(0)
  let worstFrame = $state(0)

  // ---- metrics popover + gateway control -------------------------------
  // The footer used to spell every number out and still not answer "how much
  // context is left". One button opens the whole readout; the space it frees
  // carries the thing people actually toggle.
  let lastUsage = $state<{
    prompt: number
    cached: number
    completion: number
    ttft: number
    draftN: number
    draftOk: number
  } | null>(null)
  let turnMs = $state(0)
  let turnStartTs = 0
  /** Context ceiling as the endpoint states it. null means it never said. */
  const ctxLimit = $derived(
    models.find((m) => m.id === model)?.context_tokens ??
      models[0]?.context_tokens ??
      null,
  )
  const ctxPct = $derived(
    ctxLimit && hostState?.context_tokens
      ? Math.round((hostState.context_tokens / ctxLimit) * 100)
      : 0,
  )
  // 269 of 262k is 0.1%, and "0%" reads as "nothing used" rather than "a
  // fraction of one percent". Say it is under one rather than round it away.
  const ctxLabel = $derived(
    !ctxLimit
      ? '\u2014'
      : !hostState?.context_tokens
        ? '0%'
        : ctxPct < 1
          ? '<1%'
          : `${ctxPct}%`,
  )
  // A measured zero is not the same as an unmeasured one: 0/1423 means the cache
  // missed, which is useful information, while "no turn has run" is not.
  const cacheLabel = $derived(promptTokens > 0 ? `${cacheHit.toFixed(0)}%` : '\u2014')

  let gw = $state<GatewayStatus | null>(null)
  let gwErr = $state('')
  let gwBusy = $state(false)
  // What the *next* start should bind. Loopback is the safe default; the app
  // only reaches for 0.0.0.0 when the user picks "any device", and the gateway
  // still refuses that bind outright if no token exists.
  let gwScope = $state<'loopback' | 'lan'>('lan')

  const gwText = $derived(
    transportKind !== 'tauri'
      ? 'you are on it'
      : !gw
        ? 'unknown'
        : gw.running
          ? `:${gw.port} · ${gw.clients} device${gw.clients === 1 ? '' : 's'}`
          : 'off',
  )
  const gwDot = $derived(
    transportKind !== 'tauri' ? 'live' : gw?.running ? 'live' : 'off',
  )

  async function loadGateway() {
    if (transportKind !== 'tauri') {
      gw = null
      return
    }
    try {
      gw = await invoke<GatewayStatus>('gateway_status')
      gwErr = ''
    } catch (e) {
      gwErr = cleanErr(`${e}`)
    }
  }

  async function setGateway(on: boolean) {
    if (gwBusy) return
    gwBusy = true
    gwErr = ''
    try {
      gw = on
        ? await invoke<GatewayStatus>('gateway_start', {
            port: gw?.port ?? 8099,
            lan: gwScope === 'lan',
          })
        : await invoke<GatewayStatus>('gateway_stop')
      pageLog(
        `gateway ${on ? 'on' : 'off'}: ${gw?.running ? `${gw.host}:${gw.port}` : 'stopped'}`,
      )
    } catch (e) {
      // The reason matters more than the state here: a refused bind looks
      // identical to a toggle that did nothing unless it is put on screen.
      gwErr = cleanErr(`${e}`)
      pageLog(`gateway ${on ? 'start' : 'stop'} failed: ${gwErr}`)
      await loadGateway()
    }
    gwBusy = false
  }

  async function setScope(which: 'loopback' | 'lan') {
    if (gwScope === which) return
    gwScope = which
    // Scope is a bind address, so changing it live means a new listener. Doing
    // it in one action beats telling the user to toggle twice.
    if (gw?.running) {
      await setGateway(false)
      await setGateway(true)
    }
  }

  let copiedUrl = $state('')
  /** The honest sentence behind the number. A count nobody can check is a
      count nobody should trust - so the addresses are printed next to it. */
  function attachedLine(g: GatewayStatus | null): string {
    const devs = g?.clients ?? 0
    const conns = g?.connections ?? 0
    if (!devs) return 'nothing attached'
    const who = (g?.peers ?? []).join(', ')
    const extra = conns !== devs ? ` (${conns} connections)` : ''
    return `${devs} device${devs === 1 ? '' : 's'}${who ? `: ${who}` : ''}${extra}`
  }

  async function copyUrl(u: string) {
    try {
      await navigator.clipboard.writeText(u)
      copiedUrl = 'copied'
      setTimeout(() => (copiedUrl = ''), 1500)
    } catch {
      copiedUrl = 'select and copy manually'
      setTimeout(() => (copiedUrl = ''), 2500)
    }
  }

  /** Quick-start chips. The tools are the product and an empty box does not
      teach them, so each chip puts a real request in the composer instead of
      describing a feature. */
  const TIPS: { label: string; text: string }[] = [
    {
      label: 'summarise my workspace',
      text: 'Look at the files in my workspace and write a short summary as markdown.',
    },
    {
      label: 'build a spreadsheet',
      text: 'Make an .xlsx budget template with monthly columns, formulas and a totals row.',
    },
    {
      label: 'untangle a folder',
      text: 'Find the duplicate and stray files here, group them, and tell me what you would delete.',
    },
    {
      label: 'research and cite',
      text: 'Research this, cite the sources you used, and save it as a document: ',
    },
  ]

  function seedTip(text: string) {
    draft = text
    menu = null
    queueMicrotask(() => {
      composerEl?.focus()
      composerEl?.setSelectionRange(text.length, text.length)
    })
  }
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
  /// Whether the turn currently in flight is one *this* client started. Only
  /// that case is allowed to hold `busy` against the host's answer: see
  /// `syncBusy`.
  let turnOurs = false
  // Events of the in-flight turn, kept so returning to its session restores the
  // partial answer instead of showing a truncated one. Bounded: a runaway stream
  // must not grow this without limit.
  let liveBuf: EngineEvent[] = []
  // A resync clears the transcript and then awaits a store read. Events that
  // arrive during that await must not be painted into a transcript that is about
  // to be replaced - they would be lost, and the replay that follows would not
  // know they had already been shown. Counted, not boolean: resyncs can nest.
  let resyncing = 0
  let resyncSeq = 0
  // Empty view = draft, or a session with nothing in it yet.
  const showEmpty = $derived(viewId === null || (viewMsgCount === 0 && !busy))
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
      if (viewId !== turnSessionId || resyncing > 0) {
        // painted by the replay once the view settles
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
              // resyncView, not loadTranscript: the store lags a live turn, so
              // a plain reload repaints from a history that does not contain the
              // turn in flight and wipes the user's own message. resyncView
              // replays it from the buffer instead.
              await resyncView(ev.d.session_id)
            } catch (e) {
              pageLog(`session_messages after switch failed: ${e}`)
            }
            await loadSessions()
          })()
        }
        break
      case 'turnStart':
        model = ev.d.model
        if (turnSessionId === null) turnSessionId = viewId
        busy = true
        turnStartTs = performance.now()
        turnMs = 0
        // Clear the previous turn's numbers. Leaving them up is how a bar reads
        // as "measuring" while nothing has arrived yet for this turn.
        curDecode = 0
        ttft = 0
        liveEstimate = false
        thinkingLive = false
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
      case 'turnFiles':
        pageLog(`turn files: ${ev.d.files.map((f) => f.path).join(', ')}`)
        transcript?.addFiles(ev.d.files)
        break
      case 'questionRequested':
        // The turn is parked until this is answered, so the answer goes straight
        // back; nothing else in the UI needs to know.
        pageLog(`question asked: ${ev.d.question}`)
        transcript?.showQuestion(ev.d, (r) => {
          void invoke('respond_question', {
            id: ev.d.id,
            text: r.text,
            options: r.options,
          }).catch((e) => {
            pageLog(`respond_question failed: ${cleanErr(String(e))}`)
            // The engine stopped waiting before we answered. Close the card the
            // same way the expiry event would, rather than leaving a button that
            // fails every time it is pressed.
            transcript?.expireQuestion(ev.d.id)
          })
        })
        break
      case 'questionAnswered':
        transcript?.settleQuestion(ev.d.id)
        break
      case 'questionExpired':
        // The tool stopped waiting: timeout, or the turn was cancelled with the
        // question still parked. Left alone, the card looks answerable forever.
        transcript?.expireQuestion(ev.d.id)
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
      case 'turnStats': {
        // Live estimates. The server's own token counts only land with `usage`,
        // so without these the bar sits at em-dashes through the whole of a long
        // first turn - which is exactly when it is being looked at.
        const st = ev.d
        if (st.ttft_ms > 0) ttft = st.ttft_ms
        if (st.est_decode_tps > 0) curDecode = st.est_decode_tps
        liveEstimate = true
        thinkingLive = st.thinking
        break
      }
      case 'usage': {
        const s = statsFromTurn(ev.d)
        rolling.push(s)
        // The server has spoken: stop labelling the rate as an estimate.
        liveEstimate = false
        thinkingLive = false
        curPrefill = s.effectivePrefillTps
        avgPrefill = rolling.avgRawPrefillTps()
        avgDecode = rolling.avgDecodeTps()
        cacheHit = s.cacheHitPct
        cachedTokens = ev.d.cached_tokens
        promptTokens = ev.d.prompt_tokens
        specPct = s.specAcceptPct
        ttft = ev.d.ttft_ms
        lastUsage = {
          prompt: ev.d.prompt_tokens,
          cached: ev.d.cached_tokens,
          completion: ev.d.completion_tokens,
          ttft: ev.d.ttft_ms,
          draftN: ev.d.draft_n,
          draftOk: ev.d.draft_accepted,
        }
        if (turnStartTs) turnMs = Math.round(performance.now() - turnStartTs)
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
        turnOurs = false
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
      connNote = ''
    } catch (e) {
      ipc = { ok: false, msg: String(e).slice(0, 140) }
      // On a phone there is no devtools. Whatever the server said has to be on
      // the screen, or "no model" sends you off to debug the wrong thing.
      connNote = String(e)
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
  let wsErr = $state('')
  let wsEditing = $state(false)
  let wsDraft = $state('')
  // Derived from the host, not from local UI state. It was declared and rendered
  // but never assigned anywhere, so the composer said "Choose workspace" even
  // when a workspace was configured - and on the gateway that read as "this is my
  // own separate session with no workspace".
  const shortWorkspace = $derived(
    hostState?.workspace ? hostState.workspace.replace(/\/+$/, '').split('/').pop() || hostState.workspace : '',
  )
  let theme = $state<'light' | 'dark'>('light')
  let attachErr = $state('')

  const THEME_KEY = 'cwoffee.theme.v1'

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
      saved = readStored(THEME_KEY)
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

  /** Retry the socket after a failure, without reloading the page. */
  async function retryAttach() {
    connNote = ''
    await attachChannel()
    if (connNote) return
    if (transportKind === 'gateway' && (await bootstrapGateway())) return
    await Promise.all([refreshState(), loadSessions(), loadModels()])
    await adoptHostSession()
  }

  async function chooseWorkspace() {
    if (wsBusy) return
    // A phone cannot open a folder picker on a machine it is not sitting at, so
    // in gateway mode the control becomes a path field instead of doing nothing.
    if (transportKind !== 'tauri') {
      wsDraft = hostState?.workspace ?? ''
      wsEditing = true
      return
    }
    wsBusy = true
    try {
      const picked = (await invoke('pick_workspace')) as string | null
      if (picked) await applyWorkspace(picked)
    } catch (e) {
      wsErr = cleanErr(e)
      pageLog(`pick_workspace failed: ${String(e)}`)
    } finally {
      wsBusy = false
    }
  }

  /** Focus an element on mount. `autofocus` is flagged by a11y checks and is
   * unreliable for an element that appears after the page has loaded. */
  function focusNow(el: HTMLElement) {
    requestAnimationFrame(() => el.focus())
  }

  /** A rejection rendered as "Error: the engine refused ..." reads like a stack
   * trace. The reason is the useful part, and it has to fit on a phone. */
  function cleanErr(e: unknown) {
    return String(e).replace(/^Error:\s*/, '').replace(/\s+/g, ' ').trim()
  }

  /**
   * Move the tools, then make the label reflect what the engine took.
   *
   * The command used to be fire-and-forget: the engine could refuse a path and
   * the only trace was a warning block in the transcript, so the control snapped
   * back to the old directory with no explanation. Now the call waits for the
   * engine to confirm, and a refusal is shown next to the control.
   */
  async function applyWorkspace(path: string) {
    if (!path) return
    wsErr = ''
    wsBusy = true
    try {
      const applied = await invoke<string>('set_workspace', { path })
      pageLog(`workspace applied: ${applied}`)
      wsEditing = false
      wsDraft = ''
    } catch (e) {
      wsErr = cleanErr(e)
      pageLog(`set_workspace failed: ${String(e)}`)
    } finally {
      wsBusy = false
      // Re-read the host rather than trusting the string we sent: the label has
      // to name the directory the tools are actually confined to.
      await refreshState()
    }
  }

  /**
   * Pull image files out of a paste event.
   *
   * `clipboardData.files` is the obvious source, but WebKitGTK frequently hands
   * over an empty list and puts the image under `items` instead. Reading only
   * `files` is why pasting a screenshot looked like nothing happened. When both
   * are empty we log what the clipboard actually contained, because "paste does
   * nothing" with no diagnostic is unfixable from the outside.
   */
  function filesFromTransfer(dt: DataTransfer | null): File[] {
    if (!dt) return []
    const direct = Array.from(dt.files ?? [])
    if (direct.length) return direct
    const fromItems: File[] = []
    for (const it of dt.items ?? []) {
      const kind = typeof it.kind === 'function' ? it.kind : (it as any).kind
      const isImage = (it.type ?? '').startsWith('image/')
      if (kind !== 'file' && !isImage) continue
      const f = typeof it.getAsFile === 'function' ? it.getAsFile() : null
      if (f) fromItems.push(f)
    }
    if (!fromItems.length && (dt.types?.length ?? 0) > 0) {
      pageLog(`paste carried no files; clipboard types=[${Array.from(dt.types).join(',')}]`)
    }
    return fromItems
  }

  function onPaste(e: ClipboardEvent) {
    const files = filesFromTransfer(e.clipboardData)
    if (!files.length) return
    e.preventDefault()
    void addFiles(files)
  }

  let dragIdx = $state<number | null>(null)
  let dropIdx = $state<number | null>(null)

  function moveAttachment(from: number, to: number) {
    if (from === to || from < 0 || to < 0 || from >= pending.length || to >= pending.length) return
    const next = [...pending]
    const [moved] = next.splice(from, 1)
    next.splice(to, 0, moved)
    pending = next
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

  async function waitTurnDone(prevTurns: number, budgetMs = 150000): Promise<boolean> {
    const dl = performance.now() + budgetMs
    while (performance.now() < dl) {
      const st = await invoke<HostState>('session_state')
      if (st.turns > prevTurns) return true
      await new Promise((r) => setTimeout(r, 250))
    }
    // Say what was waited for and for how long. Two checks used to fail here with
    // "turn never finished" and no way to tell a hang from a slow model; the
    // endpoint serves one slot and a 60s first token has been measured under
    // contention, so the budget has to sit above that and the timeout has to name
    // itself when it still trips.
    pageLog(`waitTurnDone: no new turn after ${budgetMs}ms (turns=${prevTurns})`)
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
    // First message of a draft: create the row now, before the turn, so the
    // host writes into it. new_session also queues the host switch, and the
    // command queue preserves the order, so start_turn lands after it.
    if (viewId === null) {
      try {
        // The folder the `+` on a workspace row preselected. Empty means "wherever
        // the host is now", which is the old behaviour.
        const id = await invoke<number>('new_session', {
          title: 'new session',
          workspace: draftWorkspace || null,
        })
        viewId = id
        draftWorkspace = ''
      } catch (e) {
        pageLog(`new_session on first send failed: ${e}`)
        return
      }
    }
    const shown = pending.map((a) => ({ mime: a.mime, name: a.name, bytes: a.bytes }))
    // Ownership and the replay copy are recorded *before* anything is painted.
    // A sessionChanged event for the row we just created can be delivered at any
    // await below; if it lands after the message is painted but before ownership
    // is recorded, the resync it triggers wipes the message and cannot put it
    // back.
    turnSessionId = viewId
    turnOurs = true
    sentDraft = { text, atts: shown }
    liveBuf = []
    transcript?.addUser(text, shown)
    // The view is no longer empty the moment a message exists, and a turn does
    // not go through loadTranscript, so record it here rather than waiting for
    // a reload that will not come.
    viewMsgCount = Math.max(viewMsgCount, 1)
    clearAttachments()
    try {
      await invoke('start_turn', { text, attachments: atts })
      pageLog(`turn sent with ${atts.length} attachment(s)`)
    } catch (e) {
      pageLog(`start_turn failed: ${e}`)
      // Nothing is running, so this client must stop claiming otherwise: with
      // `turnOurs` left set, `syncBusy` refuses to correct `busy` and a later
      // host-state read cannot undo the mistake.
      turnOurs = false
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

  /**
   * Reconcile the local "a turn is running" flag with the host's.
   *
   * A client can end up believing a turn is still running when it is not: it
   * received `turnStart`, then the socket hiccuped or it fell behind the event
   * stream and never got the `turnEnd`. The symptom is a composer that stays
   * disabled with nothing computing, which reads as a frozen app. Only the host
   * knows whether a turn is up, so for any turn this client did not start, the
   * host's answer wins.
   */
  function syncBusy(h: HostState) {
    // A turn this client started is trusted locally: the host flips `busy` a
    // command-queue hop after we send, and reading false in that window would
    // re-enable the composer mid-send.
    if (turnOurs) return
    if (busy !== h.busy) {
      pageLog(`busy corrected ${busy}->${h.busy} from host state`)
      // A stale `turnSessionId` misroutes the live buffer, so a client that
      // missed the end of a turn has to let go of it too.
      turnSessionId = null
      liveBuf = []
      busy = h.busy
    }
  }

  async function refreshState() {
    // The pill says "on :8099 · 2 devices"; a value cached at boot would be a
    // guess within a minute. Cheap local read, so refresh it with the rest.
    void loadGateway()
    try {
      hostState = await invoke<HostState>('session_state')
      if (hostState) {
        mode = hostState.mode
        thinking = hostState.reasoning
        model = hostState.model
        syncBusy(hostState)
      }
    } catch (e) {
      ipc = { ok: false, msg: String(e).slice(0, 140) }
    }
  }

  /**
   * Show the session the host is actually on.
   *
   * Deliberately does NOT call switch_session: a watch-only client must not be
   * able to move the host's session by loading a page, and opening a view is not
   * a state change. The host's session is the one the phone should be looking at.
   */
  async function adoptHostSession() {
    const open = hostState?.session_id ?? 0
    // The host has a session open: look at that one.
    if (open > 0) {
      await resyncView(open)
      return
    }
    // The host is on a fresh draft (right after a restart, session_id is 0).
    // Showing the most recent real conversation is the faithful reading of "use
    // the host": a phone that opens onto a blank screen looks like its own
    // separate empty session, which is exactly what it was reported as. Still
    // read-only - viewing is not switching.
    const recent = sessions.find((s) => s.message_count > 0)
    if (recent) await resyncView(recent.id)
  }

  /**
   * Everything a cold client needs, in one round trip.
   *
   * Boot used to be four RPCs. Each costs the server under 2ms, but a phone on
   * the measured path (Tailscale to an iOS device, ~200-400ms RTT and lossy)
   * pays a full round trip for each one, so the number of calls - not the work -
   * is what feels slow. Returns false when the server has no such command, so
   * boot falls back to the individual calls rather than showing nothing.
   */
  async function bootstrapGateway(): Promise<boolean> {
    try {
      const b = await invoke<{
        state: HostState
        sessions: Session[]
        models: ModelInfo[]
        session_id: number | null
        page: { messages: any[]; has_older: boolean }
        workspaces?: WorkspaceRow[]
      }>('bootstrap')
      hostState = b.state
      if (hostState) {
        mode = hostState.mode
        thinking = hostState.reasoning
        model = hostState.model
        syncBusy(hostState)
      }
      sessions = b.sessions ?? []
      recordSeen(sessions)
      applyUnread(sessions)
      // The folder list arrives with the sessions rather than as a fifth call.
      workspaces = b.workspaces ?? []
      models = b.models ?? []
      modelErr = ''
      const page = b.page ?? { messages: [], has_older: false }
      if (b.session_id) {
        // The server picked which session to show using the same rule this client
        // would have applied, so the transcript arrives with the rest.
        viewId = b.session_id
        transcript?.clear()
        transcript?.loadHistory(page.messages)
        viewMsgCount = page.messages.length
        hasOlder = page.has_older
        oldestId = page.messages.length ? page.messages[0].id : null
      }
      return true
    } catch (e) {
      pageLog(`bootstrap failed, falling back to separate calls: ${String(e)}`)
      return false
    }
  }

  async function loadSessions() {
    // Fired alongside, not awaited: the folder list is a second read of the same
    // store, and on a phone link waiting for it in series would add a whole round
    // trip to every refresh for information that is not on the critical path.
    void loadWorkspaces()
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
    if (matchMedia('(max-width: 767px)').matches) setSidebar(false)
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
    const mySeq = ++resyncSeq
    resyncing++
    viewId = id
    transcript?.clear()
    try {
      await loadTranscript(id)
    } catch (e) {
      pageLog(`session_messages failed: ${e}`)
    }
    resyncing--
    // A newer resync has already taken the view; painting this one's result
    // would show a session the user has since moved away from.
    if (mySeq !== resyncSeq) return
    // Returning to the session that owns a running turn: the stored history
    // does not contain that turn yet, so replay what we buffered.
    if (turnSessionId === id && sentDraft) {
      transcript?.addUser(sentDraft.text, sentDraft.atts)
      for (const e of liveBuf) transcript?.apply(e)
    }
    await refreshState()
  }

  // Draft, not a new row: a fresh session is a blank view. The row appears when
  // the first message is sent, already titled by that message.
  function newSession() {
    viewId = null
    viewMsgCount = 0
    hasOlder = false
    oldestId = null
    draftWorkspace = ''
    transcript?.clear()
    try {
      invoke('clear_session')
    } catch (e) {
      pageLog(`clear_session failed: ${e}`)
    }
    void loadSessions()
    setTimeout(() => composerEl?.focus(), 0)
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

  /// Perform the delete. Confirmation is the caller's business.
  ///
  /// This used to carry its own two-step confirm, and so did the row menu. The
  /// menu set `confirmDeleteId`, then called this, which cleared it and saw
  /// "not yet confirmed" - so it re-armed the prompt and returned without
  /// deleting. Every delete from the menu was a no-op. One owner, one confirm.
  async function removeSession(id: number) {
    try {
      await invoke('delete_session', { id })
      await loadSessions()
      await refreshState()
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
      models = await invoke<ModelInfo[]>('list_models')
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
    viewMsgCount = 0
    const page = await invoke<{ messages: any[]; has_older: boolean }>('session_messages', {
      id,
    })
    transcript?.loadHistory(page.messages)
    // A queued session switch can land before the turn has persisted anything,
    // and reloading then would report an in-flight session as empty and drop
    // the view back to the greeting mid-turn.
    viewMsgCount = turnSessionId === id ? Math.max(page.messages.length, 1) : page.messages.length
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
      // Sessions created lazily by a check's `newSession(); send()` pair are not
      // the `selftest` session this harness deletes by id, and auto-titling renames
      // them anyway, so an exact-title delete never found them. Ten rows of debris
      // per run was the result. Sweep by the marker every prompt here starts with.
      async function sweepSelftest(where: string, overId = 0) {
        const all = await invoke<Session[]>('list_sessions')
        // Titles are useless for this: a session renames itself from its first
        // message, so the harness's own sessions end up titled "Use the exec tool
        // to ..." and a title filter quietly sweeps nothing while leaving
        // turn-after-turn of debris in the user's sidebar. Ids are monotonic, so
        // everything created after the run started is the run's own doing.
        const junk = all.filter(
          (s) => s.id > overId || /^selftest/i.test(s.title),
        )
        for (const s of junk) await invoke('delete_session', { id: s.id }).catch(() => {})
        if (junk.length) pageLog(`selftest sweep (${where}): removed ${junk.length} row(s)`)
        return junk.length
      }

      // Pin the settings the harness depends on, and put them back afterwards.
      // Two problems this closes: a check that runs under the user's `ask` mode
      // parks on an approval nobody answers (the model sometimes calls `exec` for
      // "count from one to five", and that turned a passing gate into a 150s
      // timeout), and the harness used to leave `mode` and `reasoning` changed
      // after itself, quietly demoting whatever the user was running.
      let mode0 = 'ask'
      let reasoning0 = 'xhigh'
      async function restoreSettings() {
        await invoke('set_mode', { mode: mode0 }).catch(() => {})
        await invoke('set_reasoning', { level: reasoning0 }).catch(() => {})
        pageLog(`selftest restored mode=${mode0} reasoning=${reasoning0}`)
      }

    try {
      {
        const st0 = await invoke<HostState>('session_state')
        mode0 = st0.mode
        reasoning0 = st0.reasoning
        await invoke('set_mode', { mode: 'auto' })
        pageLog(`selftest pinned mode=auto (was ${mode0}, reasoning ${reasoning0})`)
      }
      const idFloor = Math.max(0, ...(await invoke<Session[]>('list_sessions')).map((s) => s.id))
      await sweepSelftest('start', idFloor)
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
      const ms = await invoke<ModelInfo[]>('list_models')
      if (!ms.length) fail('list_models returned nothing')
      const pick = ms[0].id
      // The capabilities are the point of the richer picker: an endpoint that
      // reports them must surface them, and one that does not must be honest
      // about it rather than showing blank chips.
      const caps = ms.map((m) => {
        const v = visionOf(m)
        return `${m.id}[ctx=${m.context_tokens ?? '?'} vision=${v === null ? '?' : v ? 'yes' : 'no'} tools=${m.tools === null ? '?' : m.tools ? 'yes' : 'no'}]`
      })
      pageLog(`models: ${caps.join(' ')}`)
      await invoke('set_model', { model: pick })
      const st = await invoke<HostState>('session_state')
      if (st.model !== pick) fail(`set_model did not stick: state=${st.model} wanted=${pick}`)

      // Opening a stored session must change what this window shows *without*
      // moving the host. View ownership and turn ownership are separate on
      // purpose: a phone tab browsing history must not be able to redirect
      // where the next turn goes. invoke() resolving only proves the command
      // was accepted, so the observable effects are what gets asserted.
      const stNow = await invoke<HostState>('session_state')
      // Exclude the view as well as the host session: comparing a session against
      // itself is how this step reported "DOM unchanged" while doing nothing.
      const other = after.find(
        (s) => s.id !== stNow.session_id && s.id !== viewId && (s.message_count ?? 0) > 0,
      )
      if (!other) fail('no stored session with messages available to open')
      else {
        const domBefore = (rootEl?.textContent ?? '').trim()
        await openSession(other.id)
        if (viewId !== other.id) fail(`view did not move: viewId=${viewId} wanted=${other.id}`)
        let domChanged = false
        const d2 = performance.now() + 3000
        while (performance.now() < d2) {
          await new Promise((r) => setTimeout(r, 100))
          if ((rootEl?.textContent ?? '').trim() !== domBefore) {
            domChanged = true
            break
          }
        }
        if (!domChanged) fail(`transcript DOM unchanged after opening session ${other.id}`)
        // On the desktop, clicking a row *is* choosing where the next turn goes,
        // so the host following the view is correct there. What must never happen
        // is the host landing on some third session, and the other direction -
        // a remote client moving the host by loading a page - is covered by the
        // gateway's own tests, where bootstrap and resync are read-only and a
        // watch token cannot drive at all.
        const stAfter = await invoke<HostState>('session_state')
        if (stAfter.session_id !== 0 && stAfter.session_id !== other.id) {
          fail(`host moved to neither the old nor the opened session: ${stAfter.session_id}`)
        }
        pageLog(
          `openSession(${other.id}): view moved, host ${stNow.session_id}->${stAfter.session_id}, ` +
            `transcript reloaded=${domChanged}`,
        )
      }

      // The session row menu: opens, offers rename and delete, fits in the
      // window, and closes on an outside click.
      {
        const more = document.querySelector('.sessrow .iconbtn.more') as HTMLElement | null
        if (!more) {
          // Say what *is* there. "no button" told nothing about whether the
          // sidebar was closed, the list was empty, or the class name moved.
          fail(
            `no session row action button rendered (rows=${
              document.querySelectorAll('.sessrow').length
            }, list=${sessions.length}, sidebar=${
              document.querySelector('.sidebar')?.classList.contains('open')
            })`,
          )
        }
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
          // Select the probe, not just "some attachment on the page". The page
          // is the newest 80 rows of a session that may already hold images
          // from earlier turns, and asserting on whichever came first made this
          // check pass or fail depending on unrelated history.
          const all = withAtt.flatMap((m: any) => m.attachments)
          const a = all.find((x: any) => x.name === 'probe.png')
          if (!a) fail(`probe attachment missing: ${JSON.stringify(all.slice(0, 3))}`)
          else if (a.bytes < 60 || a.bytes > 80) fail(`attachment size wrong: ${a.bytes}`)
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
        await invoke('start_turn', { text: 'Use the exec tool to run: echo cwoffee-ok' })
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
      // That block needed `ask`; nothing after it does. Leaving the mode there
      // means the next check that happens to make the model reach for `exec`
      // parks on an approval nobody is listening for, and the check reads as a
      // hung turn rather than a harness that changed the rules three steps back.
      await invoke('set_mode', { mode: 'auto' })

      // Workspace switching. The engine is the authority on what a workspace may
      // be, so a bad path must be refused *and* the previous one kept.
      const wsBefore = (await invoke<HostState>('session_state')).workspace
      // The refusal is the point, and it arrives as a rejection now. Not catching
      // it here aborts the whole suite with the engine's message as the failure,
      // which is exactly the misleading report this harness exists to avoid.
      let refused = ''
      try {
        await invoke('set_workspace', { path: '/nonexistent/definitely-not' })
        fail('a non-directory workspace was accepted')
      } catch (e) {
        refused = cleanErr(e)
      }
      if (!refused) fail('the refusal carried no reason')
      await new Promise((r) => setTimeout(r, 500))
      const wsBad = (await invoke<HostState>('session_state')).workspace
      if (wsBad !== wsBefore) fail(`a non-directory workspace was adopted: ${wsBad}`)
      pageLog(`workspace refusal: ${refused}`)

      await invoke('set_workspace', { path: '/tmp' })
      await new Promise((r) => setTimeout(r, 500))
      const wsTmp = (await invoke<HostState>('session_state')).workspace
      if (wsTmp !== '/tmp') fail(`workspace switch did not land: ${wsTmp}`)

      await invoke('set_workspace', { path: wsBefore })
      // Undo the sidebar entry the /tmp switch created. A check that leaves a
      // "tmp" folder in the user's sidebar forever is the same mistake as the
      // one that left their thinking effort set to low: the harness borrowed
      // state, so it has to give it back.
      await invoke('remove_workspace', { path: '/tmp' }).catch(() => {})
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
      await invoke('delete_session', { id: sessA })
      await invoke('delete_session', { id: sessB })

      // --- draft sessions ----------------------------------------------------
      // "New session" must not leave an empty row behind, and the row must
      // appear only when the first message is sent - already titled by it.
      {
        const beforeDraft = await invoke<Session[]>('list_sessions')
        newSession()
        await new Promise((r) => setTimeout(r, 300))
        const afterDraft = await invoke<Session[]>('list_sessions')
        if (afterDraft.length !== beforeDraft.length) {
          fail(`new session created a row: ${beforeDraft.length} -> ${afterDraft.length}`)
        }
        if (!showEmpty) fail('draft view did not present the empty state')

        // Deliberately tool-free: a prompt that tempts the model into a tool
        // call would park this step on an approval nobody is there to answer.
        draft = 'What is 6 times 7? Answer with just the number.'
        // Read the turn counter *before* sending. Reading it after can already
        // be satisfied by the turn we are waiting for, which makes the wait a
        // no-op and the assertions below race persistence.
        const turnsBeforeDraft = (await invoke<HostState>('session_state')).turns
        await send()
        if (!(await waitTurnDone(turnsBeforeDraft))) {
          fail('draft first turn never finished')
        }
        const afterSend = await invoke<Session[]>('list_sessions')
        if (afterSend.length !== beforeDraft.length + 1) {
          fail(`first message did not create exactly one row: ${beforeDraft.length} -> ${afterSend.length}`)
        }
        const mine = afterSend.find((x) => x.title.includes('6 times 7'))
        if (!mine) fail(`the new row was not titled by its first message: ${afterSend.slice(0,3).map((x)=>x.title).join(' | ')}`)
        if (!(await waitFor('view to leave the empty state', () => !showEmpty, 5000))) {
          fail(`view stayed empty after sending (viewId=${viewId} msgCount=${viewMsgCount} busy=${busy})`)
        }
        if (mine) await invoke('delete_session', { id: mine.id })
      }

      // --- delete through the actual DOM, the way a user does it ------------
      {
        const doomed = await invoke<number>('new_session', { title: 'selftest doomed' })
        await loadSessions()
        await new Promise((r) => setTimeout(r, 300))
        const row = [...document.querySelectorAll('.sessrow')].find((r) =>
          r.textContent?.includes('selftest doomed'),
        )
        if (!row) {
          fail('the doomed session row was not rendered')
        } else {
          const dots = row.querySelector('.iconbtn.more') as HTMLElement | null
          if (!dots) {
            fail('no 3-dots control on the session row')
          } else {
            dots.click()
            await new Promise((r) => setTimeout(r, 350))
            const items = () =>
              [...document.querySelectorAll('.pop .pop-item')] as HTMLElement[]
            let del = items().find((b) => b.textContent?.trim() === 'Delete')
            if (!del) {
              fail(`row menu did not offer Delete (items: ${items().map((b) => b.textContent?.trim()).join('|')})`)
            } else {
              del.click()
              await new Promise((r) => setTimeout(r, 350))
              del = items().find((b) => b.textContent?.trim() === 'Delete')
              if (!del) {
                fail('two-step confirm never showed a second Delete button')
              } else {
                del.click()
                await new Promise((r) => setTimeout(r, 600))
                const still = await invoke<Session[]>('list_sessions')
                if (still.some((x) => x.id === doomed)) fail('session survived the delete flow')
              }
            }
          }
        }
      }

      // --- the message you just sent must survive the session being created --
      // This is the video bug: sending from a draft wiped the user's own message
      // and opened on a blank transcript, because the new session announced
      // itself and the handler reloaded from a store that did not contain the
      // turn yet.
      {
        newSession()
        draft = 'Selftest wipe check: reply with the single word ok.'
        const turnsBeforeWipe = (await invoke<HostState>('session_state')).turns
        await send()
        // Assert while the turn is in flight, not after it settles: after the
        // turn the store catches up and the bug is invisible.
        const mine = () =>
          [...document.querySelectorAll('.blk-user .blk-body')].some((u) =>
            u.textContent?.includes('Selftest wipe check'),
          )
        // Waited, not sampled: a resync is legitimately in flight here. What must
        // never happen is the message staying missing.
        if (!(await waitFor('sent message to be in the transcript', mine, 4000))) {
          const detail = [...document.querySelectorAll('.transcript .blk')]
            .map((b) => `${b.className} :: ${(b.textContent ?? '').slice(0, 90)}`)
            .join(' || ')
          fail(
            `sent message vanished from the transcript (viewId=${viewId} turnId=${turnSessionId} blocks: ${detail})`,
          )
        }
        if (!(await waitTurnDone(turnsBeforeWipe))) fail('wipe-check turn never finished')
      }

      // --- the live marker: one during the turn, none after ------------------
      {
        const liveCount = () => document.querySelectorAll('.transcript .blk.live').length
        newSession()
        draft = 'Selftest live marker: count from one to five.'
        const turnsBeforeLive = (await invoke<HostState>('session_state')).turns
        await send()
        // Wait for output to actually be streaming, then look.
        if (!(await waitFor('a live block to appear', () => liveCount() === 1, 6000))) {
          fail(`expected exactly one live block during a turn, saw ${liveCount()}`)
        }
        if (!(await waitTurnDone(turnsBeforeLive))) {
          fail('live-marker turn never finished')
        } else if (liveCount() !== 0) {
          fail(`live marker survived the turn: ${liveCount()} still marked`)
        }
      }

      // --- thinking label + markdown tables ---------------------------------
      {
        await invoke('set_reasoning', { level: 'medium' })
        draft =
          'Reply with ONLY a markdown table, no prose. Two columns named name and value, ' +
          'two data rows: alpha 1 and beta 2.'
        const turnsBefore = (await invoke<HostState>('session_state')).turns
        await send()
        if (!(await waitTurnDone(turnsBefore))) {
          fail('table turn never finished')
        } else {
          await new Promise((r) => setTimeout(r, 400))

          const table = document.querySelector('.transcript table')
          if (!table) {
            fail('markdown table was not rendered as a table')
          } else {
            if (!table.querySelector('thead th')) fail('table has no header cells')
            if (table.querySelectorAll('tbody tr').length < 2)
              fail(`table has too few body rows: ${table.querySelectorAll('tbody tr').length}`)
            if (table.textContent?.includes('|')) fail('stray pipes left in the table')
          }

          const rblk = document.querySelector('.blk-reasoning') as HTMLElement | null
          if (!rblk) {
            fail('no thinking block in the turn')
          } else {
            const head = rblk.querySelector('.blk-head') as HTMLElement
            const body = rblk.querySelector('.blk-body') as HTMLElement
            const label = head.textContent ?? ''
            if (!/^thought for /.test(label))
              fail(`thinking label was not replaced with a duration: ${JSON.stringify(label)}`)
            // The arrow wrapping onto its own line was the reported bug.
            if (head.getBoundingClientRect().height > 22)
              fail(`thinking head is not a single line: ${head.getBoundingClientRect().height}px`)

            // Whole collapsed block is the click target, not just the label.
            body.click()
            await new Promise((r) => setTimeout(r, 120))
            if (!body.classList.contains('expanded')) fail('clicking the thinking body did not expand it')
            // Once open, only the label collapses it.
            body.click()
            await new Promise((r) => setTimeout(r, 120))
            if (!body.classList.contains('expanded')) fail('clicking the body while expanded collapsed it')
            head.click()
            await new Promise((r) => setTimeout(r, 120))
            if (body.classList.contains('expanded')) fail('clicking the label did not collapse it')
          }
        }
      }

      // --- files a turn produces show up as clickable attachments ----------
      {
        await invoke('set_mode', { mode: 'auto' })
        draft =
          'Use write_file to create selftest-note.md in the workspace containing exactly ' +
          'the line: cwoffee output file. Then tell me the path you wrote.'
        const turnsBefore = (await invoke<HostState>('session_state')).turns
        await send()
        if (!(await waitTurnDone(turnsBefore))) {
          fail('output-file turn never finished')
        } else {
          await new Promise((r) => setTimeout(r, 400))
          const chips = [...document.querySelectorAll('.filechip')] as HTMLElement[]
          if (!chips.length) {
            fail('no file attachment rendered for a turn that wrote a file')
          } else {
            const paths = chips.map((c) => c.dataset.path ?? '')
            if (!paths.some((p) => p.includes('selftest-note.md')))
              fail(`file chip path wrong: ${JSON.stringify(paths)}`)
            if (!chips[0].textContent?.includes('selftest-note.md'))
              fail(`file chip has no file name: ${JSON.stringify(chips[0].textContent)}`)
          }
        }
      }

      // --- code block chrome + what the preview sandbox actually blocks ----
      {
        await invoke('set_reasoning', { level: 'off' })
        draft =
          'Reply with ONLY a fenced html code block. Inside it put exactly: ' +
          '<b>bold</b> and a <script>window.__pwn=1<\/script>. No prose.'
        const turnsBefore = (await invoke<HostState>('session_state')).turns
        await send()
        if (!(await waitTurnDone(turnsBefore))) {
          fail('code block turn never finished')
        } else {
          await new Promise((r) => setTimeout(r, 400))
          const blk = document.querySelector('.codeblk[data-lang="html"]') as HTMLElement | null
          if (!blk) fail('html fence did not render as a code block with chrome')
          else {
            if (!blk.querySelector('.code-act[data-act="copy"]')) fail('code block has no copy button')
            if (!blk.querySelector('.code-act[data-act="preview"]')) fail('html block has no preview button')
            // The fence must not execute: it is escaped markup, not live HTML.
            if (document.querySelector('.transcript b'))
              fail('html inside a code fence was rendered as live markup')
          }

          // What is the actual blast radius of a preview frame? If a sandboxed
          // iframe can see the IPC bridge, "preview" is a remote-code path and
          // scripts must stay off forever. Measure it instead of assuming.
          const probe = await new Promise<Record<string, unknown>>((resolve) => {
            const f = document.createElement('iframe')
            f.setAttribute('sandbox', 'allow-scripts')
            f.style.display = 'none'
            f.srcdoc =
              '<script>parent.postMessage({t: !!window.__TAURI__, i: !!window.__TAURI_INTERNALS__,' +
              ' loc: String(location.href).slice(0,40)}, "*")<\/script>'
            const on = (e: MessageEvent) => {
              window.removeEventListener('message', on)
              f.remove()
              resolve(e.data as Record<string, unknown>)
            }
            window.addEventListener('message', on)
            document.body.append(f)
            setTimeout(() => resolve({ timeout: true }), 2500)
          })
          pageLog(`preview sandbox probe: ${JSON.stringify(probe)}`)
          if (probe.t || probe.i) {
            fail('a sandboxed preview frame can reach the Tauri IPC bridge')
          }

          // The shipped preview path does allow scripts, so the thing that has
          // to hold is the injected CSP: no network at all from the frame.
          const net = await new Promise<Record<string, unknown>>((resolve) => {
            const f = document.createElement('iframe')
            f.setAttribute('sandbox', 'allow-scripts')
            f.style.display = 'none'
            f.srcdoc =
              '<meta http-equiv="Content-Security-Policy" content="default-src \'none\'; ' +
              "script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data: blob:; " +
              'font-src data:; media-src blob:">' +
              '<script>(async () => { const out = {};' +
              "for (const u of ['http://127.0.0.1:8090/health','ipc://localhost/x','https://example.com/']) {" +
              ' try { await fetch(u); out[u] = "FETCHED" } catch (e) { out[u] = String(e).slice(0,60) } }' +
              ' out.ran = 1; parent.postMessage(out, "*") })()<\/script>'
            const on = (e: MessageEvent) => {
              window.removeEventListener('message', on)
              f.remove()
              resolve(e.data as Record<string, unknown>)
            }
            window.addEventListener('message', on)
            document.body.append(f)
            setTimeout(() => resolve({ timeout: true }), 3000)
          })
          pageLog(`preview CSP probe: ${JSON.stringify(net)}`)
          const leaked = Object.entries(net).filter(
            ([k, v]) => k !== 'ran' && String(v) === 'FETCHED',
          )
          if (leaked.length) fail(`preview frame reached the network: ${JSON.stringify(leaked)}`)
          if (net.timeout) fail('preview CSP probe never reported')
        }
      }

      // Remote access, driven through the real commands. The footer pill now
      // claims the gateway is on, on a given port, with N devices attached;
      // that claim has to come from the same controller that owns the listener,
      // so the start / read / stop cycle is asserted here rather than eyeballed.
      if (transportKind === 'tauri') {
        const g0 = await invoke<{ running: boolean; port: number; clients: number }>(
          'gateway_status',
        )
        if (typeof g0.running !== 'boolean') fail('gateway_status returned no running flag')
        if (!Number.isFinite(g0.clients) || g0.clients < 0 || g0.clients > 1000)
          fail(`implausible device count before the cycle: ${g0.clients}`)
        if (g0.running) {
          // The pill is meant to be used, so a gateway is often already up when the
          // self-test runs. Driving it down would fight the user's own listener -
          // and cut a phone off mid-session - so with one already live only the
          // read path is asserted here; the start/stop cycle runs when none is.
          pageLog(
            `gateway already on :${g0.port} with ${g0.clients} device(s); skipped the start/stop cycle`,
          )
        } else {
          // A port of its own, so the cycle never touches a gateway the user is on.
          const up = await invoke<{ running: boolean; port: number; lan: boolean }>(
            'gateway_start',
            { port: 8123, lan: false },
          )
          if (!up.running) fail('gateway_start reported running=false')
          if (up.port !== 8123) fail(`gateway started on ${up.port}, asked for 8123`)
          if (up.lan) fail('gateway_start(lan:false) bound a non-loopback address')
          const g1 = await invoke<{ running: boolean; clients: number }>('gateway_status')
          if (!g1.running) fail('gateway_status disagrees with gateway_start')
          if (!Number.isFinite(g1.clients) || g1.clients > 1000)
            fail(`implausible device count ${g1.clients}`)
          const down = await invoke<{ running: boolean }>('gateway_stop')
          if (down.running) fail('gateway_stop left running=true')
          const g2 = await invoke<{ running: boolean }>('gateway_status')
          if (g2.running) fail('gateway_status still says running after stop')
          // Stopping twice must say so rather than pretend.
          try {
            await invoke('gateway_stop')
            fail('second gateway_stop should have errored')
          } catch {
            /* expected */
          }
          pageLog(`gateway cycle ok: false -> on:8123 -> off, clients=${g1.clients}`)
        }
      }

      // The question card: the turn stops while it is up, so the card has to
      // carry the question, the choices, the recommendation and a typed answer -
      // and must not fire twice.
      {
        // In an object, not a bare local: TypeScript narrows a closure-assigned
        // local to never and the assertions stop type-checking.
        const click = (n: Element | undefined | null) =>
          n?.dispatchEvent(new MouseEvent('click', { bubbles: true }))

        // --- single choice: nothing is sent until Send is pressed -------------
        const got: { v: { text: string; options: number[] } | null } = { v: null }
        transcript?.showQuestion(
          {
            id: 900_001,
            question: 'Which output should this produce?',
            options: [
              { label: 'Markdown', detail: 'editable' },
              { label: 'PDF', detail: 'prints well' },
            ],
            recommended: 1,
            allow_custom: true,
            multi: false,
          },
          (r) => (got.v = r),
        )
        let card = document.querySelector('.blk-ask')
        if (!card) fail('question card did not render')
        else if (!card.isConnected) fail('the question card was built but never attached')
        else {
          if (!card.textContent?.includes('Which output should this produce?'))
            fail('question card is missing the question')
          const opts = card.querySelectorAll('.ask-opt')
          if (opts.length !== 2) fail(`question card shows ${opts.length} options, expected 2`)
          if (card.querySelectorAll('.ask-opt.is-rec').length !== 1)
            fail('exactly one option must be badged as recommended')
          // The badge sits with the option it describes, not in its own band.
          const badge = card.querySelector('.ask-badge')
          if (!badge?.closest('.ask-opt-title')) fail('the recommended badge is not inline with its option title')
          if (!card.querySelector('.ask-input')) fail('no typed-answer affordance')
          const send = card.querySelector<HTMLButtonElement>('.ask-submit')
          if (!send) fail('no explicit submit on the question card')
          if (send?.disabled !== true) fail('send must start disabled - a question with no choice is not an answer')

          click(opts[1])
          if (got.v) fail('clicking an option answered the model on its own')
          if (send!.disabled) fail('send stayed disabled after a choice was made')
          click(send)
          if (!got.v || got.v.options.join(',') !== '1' || got.v.text !== 'PDF')
            fail(`send produced ${JSON.stringify(got.v)}`)
          // A second submit must not send a second answer: the engine has already
          // dropped the reply sender, and a second call is an error to the user.
          click(opts[0])
          click(send)
          if (got.v?.options.join(',') !== '1') fail('a second choice overwrote the first answer')
          if (!card.querySelector('.ask-answered')) fail('the answered card does not say what was sent')
          card.remove()
        }

        // --- multi choice: several boxes, and picks are kept separate ----------
        const multi: { v: { text: string; options: number[] } | null } = { v: null }
        transcript?.showQuestion(
          {
            id: 900_002,
            question: 'Which formats do you want?',
            options: [{ label: 'PDF' }, { label: 'DOCX' }, { label: 'XLSX' }],
            recommended: 0,
            allow_custom: true,
            multi: true,
          },
          (r) => (multi.v = r),
        )
        card = document.querySelector('.blk-ask')
        if (!card) fail('multi question card did not render')
        else {
          const opts = card.querySelectorAll('.ask-opt')
          click(opts[0])
          click(opts[2])
          click(opts[2]) // toggles back off
          click(card.querySelector('.ask-submit'))
          if (!multi.v || multi.v.options.join(',') !== '0')
            fail(`multi select produced ${JSON.stringify(multi.v)}`)
          if (card.querySelectorAll('.ask-opt.is-sel').length !== 1)
            fail('a second click must untick an option, not add it twice')
          card.remove()
        }

        // A question that closed with no answer must close in the view too.
        transcript?.showQuestion(
          {
            id: 900_003,
            question: 'Still there?',
            options: [{ label: 'Yes' }, { label: 'No' }],
            allow_custom: false,
            multi: false,
          },
          () => fail('an expired question must not send an answer'),
        )
        card = document.querySelector('.blk-ask:last-of-type')
        if (!card) fail('expiry question card did not render')
        else {
          transcript?.expireQuestion(900_003)
          if (!card.classList.contains('expired')) fail('an expired card does not say so')
          if (!(card.querySelector('.ask-submit') as HTMLButtonElement).disabled)
            fail('an expired card still offers Send')
          click(card.querySelector('.ask-submit'))
          card.remove()
        }

        // An answer for a question nobody asked is an error, not a shrug.
        try {
          await invoke('respond_question', { id: 900_999, text: 'x', options: [] })
          fail('respond_question accepted an id that was never asked')
        } catch {
          /* expected */
        }
        pageLog('question card ok')
      }

      // Tool headers carry the path/url/query a call is about; exec deliberately
      // does not, because its argument is a whole command.
      {
        const cases: [string, unknown, string][] = [
          ['write_file', { path: 'reports/q3.xlsx' }, 'reports/q3.xlsx'],
          ['edit_file', '{"path":"notes.md","old_text":"a"', 'notes.md'],
          ['read_file', '{"path":"deep/nested/file.py"}', 'deep/nested/file.py'],
          ['web_search', { query: 'rust tokio' }, 'rust tokio'],
          ['exec', { cmd: 'rm -rf /' }, ''],
        ]
        for (const [name, args, want] of cases) {
          const got = subjectFor(name, args)
          if (got !== want) fail(`subjectFor(${name}) = ${JSON.stringify(got)}, want ${JSON.stringify(want)}`)
        }
        // A half-streamed path must not paint a partial name and resize the row.
        if (subjectFor('write_file', '{"path":"no') !== '')
          fail('subjectFor matched an unterminated path')
        pageLog('tool header subject ok')
      }

      // The `+N -M` chip and the diff inside the card. Rendered from stored
      // history rather than from a live model turn: the engine tests cover where
      // the numbers come from, and this covers what the card does with them -
      // including the reload path, which is where a card that only works live
      // shows up as "the numbers disappeared".
      {
        transcript?.clear()
        const history: StoredMessage[] = [
          {
            id: 90_001,
            session_id: 0,
            role: 'assistant',
            content: 'editing the note',
            reasoning: '',
            tool_call_id: null,
            tool_calls_json: JSON.stringify([
              {
                id: 'c1',
                name: 'edit_file',
                arguments: '{"path":"notes.md","old_text":"one","new_text":"two"}',
              },
            ]),
            reasoning_signature: '',
            reasoning_ms: 0,
            files: [],
            created_at: 0,
            attachments: [],
          },
          {
            id: 90_002,
            session_id: 0,
            role: 'tool',
            content: '1 replacement, 12 -> 13 lines',
            tool_call_id: 'c1',
            tool_calls_json: '',
            reasoning: '',
            reasoning_signature: '',
            reasoning_ms: 0,
            files: [],
            created_at: 0,
            attachments: [],
            changes: [
              {
                path: 'notes.md',
                added: 2,
                removed: 1,
                created: false,
                diff: '@@ -1,3 +1,4 @@\n alpha\n-bravo\n+bravo two\n+charlie\n delta\n',
              },
            ],
          },
          {
            id: 90_003,
            session_id: 0,
            role: 'assistant',
            content: 'and a new file',
            reasoning: '',
            tool_call_id: null,
            tool_calls_json: JSON.stringify([
              { id: 'c2', name: 'write_file', arguments: '{"path":"fresh.md","content":"x"}' },
            ]),
            reasoning_signature: '',
            reasoning_ms: 0,
            files: [],
            created_at: 0,
            attachments: [],
          },
          {
            id: 90_004,
            session_id: 0,
            role: 'tool',
            content: 'wrote 4 lines',
            tool_call_id: 'c2',
            tool_calls_json: '',
            reasoning: '',
            reasoning_signature: '',
            reasoning_ms: 0,
            files: [],
            created_at: 0,
            attachments: [],
            changes: [{ path: 'fresh.md', added: 4, removed: 0, created: true }],
          },
        ]
        // ask_user gets no tool card in history either. A card headed `ASK_USER`
        // is a mechanism with the question missing.
        transcript?.loadHistory([
          {
            id: 90_010,
            session_id: 0,
            role: 'assistant',
            content: 'one question first',
            reasoning: '',
            tool_call_id: null,
            tool_calls_json: JSON.stringify([
              {
                id: 'c3',
                name: 'ask_user',
                arguments: '{"question":"Which format?","options":[{"label":"PDF"}],"multi":false}',
              },
            ]),
            reasoning_signature: '',
            reasoning_ms: 0,
            files: [],
            created_at: 0,
            attachments: [],
          },
          {
            id: 90_011,
            session_id: 0,
            role: 'tool',
            content: 'User chose: PDF',
            tool_call_id: 'c3',
            tool_calls_json: '',
            reasoning: '',
            reasoning_signature: '',
            reasoning_ms: 0,
            files: [],
            created_at: 0,
            attachments: [],
          },
        ])
        {
          const asked = [...document.querySelectorAll('.blk-note')].filter(
            (n) => n.textContent?.includes('Which format?'),
          )
          if (!asked.length) fail('a stored ask_user did not render as the question')
          const asTool = [...document.querySelectorAll('.blk-tool')].filter(
            (n) => n.querySelector('.tool-name')?.textContent === 'ask_user',
          )
          if (asTool.length) fail('ask_user rendered as a tool card in history')
          // The answer belongs to the same block, so it must not be dropped.
          if (!asked[0]?.textContent?.includes('User chose'))
            fail('the stored answer to ask_user went nowhere')
        }
        transcript?.clear()
        transcript?.loadHistory(history)
        const cards = document.querySelectorAll('.blk-tool')
        if (cards.length !== 2) fail(`expected 2 tool cards, saw ${cards.length}`)
        const edited = cards[0]?.querySelector('.tool-stat')
        if (!edited) fail('the edit card shows no +N -M chip')
        else if (edited.textContent !== '+2 -1')
          fail(`edit chip reads ${JSON.stringify(edited.textContent)}, want "+2 -1"`)
        if (edited && !edited.closest('.blk-tool')?.classList.contains('has-changes'))
          fail('a card with changes is not marked, so it cannot be styled')
        const adds = cards[0]?.querySelectorAll('.tool-diff .diff-add')
        if (adds?.length !== 2) fail(`diff shows ${adds?.length} added lines, want 2`)
        if (!cards[0]?.querySelector('.tool-diff .diff-del')) fail('diff shows no removed line')
        if (!cards[0]?.querySelector('.tool-diff .diff-hunk')) fail('diff has no hunk header')
        // A created file is `+4`, not `+4 -0`: there was nothing to remove.
        const created = cards[1]?.querySelector('.tool-stat')
        if (!created || created.textContent !== '+4')
          fail(`created-file chip reads ${JSON.stringify(created?.textContent)}, want "+4"`)
        if (cards[1]?.querySelector('.tool-diff'))
          fail('a brand new file should not render a diff of nothing')
        transcript?.clear()
        pageLog('file change chip ok')
      }

      // Sidebar grouping. The claim being tested is the one the user sees: every
      // session is inside a folder group, the folder the host is in is one of
      // them, and the `+` on a row preselects that folder without creating a
      // session row - sessions come into existence on the first message.
      {
        await loadWorkspaces()
        const groups = wsGroups
        if (!groups.length) fail('the sidebar has no workspace groups')
        const inside = groups.reduce((n, g) => n + g.list.length, 0)
        if (inside !== sessions.length)
          fail(`grouping holds ${inside} of ${sessions.length} sessions`)
        const cur = hostState?.workspace ?? ''
        if (cur && !groups.some((g) => g.path === cur))
          fail(`no group for the folder the tools are in (${cur})`)
        if (document.querySelectorAll('.wsgrp').length !== groups.length)
          fail('the rendered groups do not match the derived ones')

        const before = await invoke<Session[]>('list_sessions')
        const plus = document.querySelector('.ws-plus') as HTMLElement | null
        if (!plus) fail('no inline + on a workspace row')
        else {
          plus.click()
          if (!draftWorkspace) fail('the + did not preselect a folder')
          if (viewId !== null) fail('the + switched to a session instead of opening a draft')
          // Svelte flushes on a microtask, so the DOM a click produces is not
          // there on the next statement. Reading it too early is a test bug, not
          // a product bug, and it reads as one.
          await new Promise<void>((r) => requestAnimationFrame(() => r()))
          if (!document.querySelector('.empty-ws'))
            fail('the greeting does not say which folder the draft will use')
          const after = await invoke<Session[]>('list_sessions')
          if (after.length !== before.length)
            fail(`the + created ${after.length - before.length} session row(s); rows appear on first message`)
          draftWorkspace = ''
        }
        pageLog('workspace sidebar ok')

      // `busy` is client state, so it can go stale: a client that received a
      // turnStart and then lost or fell behind the event stream never sees the
      // turnEnd, and the composer stays disabled with nothing computing. That
      // reads as a frozen app, and it is invisible until someone tries to type.
      if (hostState && !hostState.busy && composerEl?.disabled)
        fail('composer is disabled while the host says no turn is running')
      }

      await sweepSelftest('end', idFloor)
      await restoreSettings()
      pageLog(
        `${fails === 0 ? 'SELFTEST ok' : `SELFTEST FAILED (${fails})`}: ` +
          `sessions ${before.length}->${after.length}, fts-safe query -> ` +
          `${hits.length} hits, models=${ms.length} picked=${pick}`,
      )
    } catch (e) {
      fail(String(e))
      pageLog(`SELFTEST FAILED (${fails}): aborted before the summary`)
      await restoreSettings()
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
      setSidebar(!sidebarOpen)
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
  // In gateway mode there is no window to control, and calling it anyway logged a
  // stack trace on every phone load - noise that reads like a real fault on a
  // device with no console open.
  if (transportKind === 'tauri') {
    try {
      win = getCurrentWindow()
    } catch (e) {
      winErr = String(e)
      console.error('[cwoffee] getCurrentWindow failed:', e)
    }
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
      empty_mode: !!document.querySelector('.empty'),
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
    if (rootEl) {
      transcript = new Transcript(rootEl)
      invoke<string[]>('migration_notes')
        .then((notes) => {
          for (const n of notes) transcript?.showNote(n)
        })
        .catch((e) => pageLog(`migration_notes failed: ${String(e)}`))
      transcript.onOpenFile = async (path) => {
        try {
          const opened = await invoke<string>('open_file', { path })
          pageLog(`opened file: ${opened}`)
        } catch (e) {
          pageLog(`open_file failed: ${e}`)
          transcript?.showError(String(e))
        }
      }
    }
    // Test seam: lets a browser harness drive the transcript without an engine.
    if (import.meta.env.DEV) (window as any).__cwoffeeTranscript = transcript
    await attachChannel()
    // A phone that drops wifi, or whose first attempt raced a sleeping machine,
    // should come back on its own rather than sit on "connecting…" until someone
    // reloads the tab.
    if (transportKind === 'gateway') {
      onReconnect(() => void retryAttach())
      // The server said "you missed events". Some of them matter - `turnEnd`
      // most of all - so re-read state instead of leaving a half-truth on screen.
      onResync(() => void retryAttach())
    }
    // One round trip in gateway mode; on a phone link this is the difference
    // between "a moment" and a wait you notice. Desktop keeps the parallel
    // calls because they are sub-millisecond over IPC.
    let booted = false
    if (transportKind === 'gateway') booted = await bootstrapGateway()
    if (!booted) {
      // The independent fetches run concurrently. They used to be awaited one
      // after another, and over a VPN each one pays a full round trip.
      const fetches: Promise<unknown>[] = [refreshState(), loadSessions(), loadModels()]
      if (transportKind === 'tauri') {
        // gpu_report is desktop-only and always errors over the gateway, so asking
        // for one there is a guaranteed-failed round trip in the middle of boot.
        fetches.push(
          invoke<GpuReport>('gpu_report')
            .then((g) => {
              gpu = g
            })
            .catch((e) => {
              gpu = null
              ipc = { ok: false, msg: String(e).slice(0, 140) }
              pageLog(`gpu_report FAILED: ${String(e)}`)
            }),
        )
      }
      await Promise.all(fetches)
      // Open the host's current session rather than a blank draft. On a phone this
      // was the whole "it's using its own session" impression: the list was there
      // but the transcript was the empty screen.
      await adoptHostSession()
    }
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

  // Guarded field by field on purpose. One missing key in a telemetry struct must
  // not take down the whole render: a gateway gpu_report stub that returned
  // `{note: ...}` threw here and blanked the app before it could load a session.
  const gpuNodes = $derived(gpu?.render_nodes ?? [])
  const gpuOk = $derived(!!gpu && gpuNodes.includes('renderD128') && !gpu.leaked_nvidia)
</script>

<div class="flex h-full flex-col bg-paper text-ink">
  <!-- ============================ titlebar ============================ -->
  <div
    class="titlebar flex h-11 shrink-0 items-center gap-2 border-b border-hairline bg-paper-2 px-3"
    data-tauri-drag-region
  >
    <img src="/icon.png" alt="" class="h-5 w-5 rounded" />
    <span class="text-[13px] font-medium tracking-tight">cwoffee</span>
    <!-- Sidebar toggle: below 768px the sidebar is a drawer, and without this
         button the session list and search are simply unreachable. -->
    <button class="iconbtn sidemenu" onclick={() => setSidebar(!sidebarOpen)} aria-label="Sessions">
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
    <!-- Mounted unconditionally and faded with a class: inserting it in the
         same frame the sidebar starts moving costs a layout+paint pass mid-slide. -->
    <button
      class="scrim"
      class:show={sidebarOpen}
      onclick={() => setSidebar(false)}
      aria-label="Close sidebar"
      aria-hidden={!sidebarOpen}
      tabindex={sidebarOpen ? 0 : -1}
    ></button>
    <aside
      class="sidebar w-[248px] shrink-0 flex-col gap-1.5 border-r border-hairline p-2.5 min-h-0"
      class:open={sidebarOpen}
      class:animating={sidebarAnimating}
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
        <!-- Folders first, sessions underneath. The folder is what people
             actually navigate by ("where did that spreadsheet go"), and a flat
             list of sessions hides it. A group with no sessions still shows: a
             folder you added and have not written in yet is information, not
             clutter. -->
        <div class="sesshead mt-2">
          <span>Workspaces</span>
          <button
            class="iconbtn ws-addall"
            aria-label="Add a folder"
            title="Add a folder to the sidebar"
            onclick={() => (wsAdding = !wsAdding)}
          >
            <svg width="12" height="12" viewBox="0 0 12 12"><path d="M6 2.6v6.8M2.6 6h6.8" stroke="currentColor" stroke-width="1.3" fill="none"/></svg>
          </button>
        </div>
        {#if wsAdding}
          <div class="wsadd">
            {#if transportKind === 'tauri'}
              <button class="pill px-2 py-1 text-[12px]" onclick={() => void addFolderPicked()}>
                Choose a folder…
              </button>
            {/if}
            <input
              class="pill px-2 py-1 text-[12px] w-full"
              placeholder={transportKind === 'tauri' ? 'or type a path' : 'host path, e.g. ~/code/app'}
              bind:value={wsAddDraft}
              onkeydown={(e) => {
                if (e.key === 'Enter') void addFolderTyped()
                else if (e.key === 'Escape') {
                  wsAdding = false
                  wsAddDraft = ''
                }
              }}
            />
            {#if wsAddErr}<div class="ws-err">{wsAddErr}</div>{/if}
          </div>
        {/if}
        {#each wsGroups as g (g.path || '\u0000none')}
          {@const open = isOpen(g.path)}
          <div class="wsgrp {open ? 'open' : 'closed'}">
            <div class="wsrow">
              {#if wsRenaming === g.path}
                <input
                  class="pill px-2 py-1 text-[12px] w-full"
                  placeholder={basenameOf(g.path)}
                  bind:value={wsLabelDraft}
                  onkeydown={(e) => {
                    if (e.key === 'Enter') commitWsRename()
                    else if (e.key === 'Escape') wsRenaming = ''
                  }}
                  onblur={() => commitWsRename()}
                />
              {:else}
                <!-- The chevron is part of the label button: folding a folder is
                     a navigation act and needs a target a thumb can hit. -->
                <button class="wsbtn" onclick={() => toggleWs(g.path)} aria-expanded={open}>
                  <svg class="ws-chev" width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
                    <path d={open ? 'M2 3.5L5 6.5L8 3.5' : 'M3.5 2L6.5 5L3.5 8'} stroke="currentColor" stroke-width="1.4" fill="none" stroke-linecap="round"/>
                  </svg>
                  <span class="ws-name" title={g.path}>{g.label}</span>
                  <span class="ws-n">{g.list.length}</span>
                </button>
                {#if g.path}
                  <button
                    class="iconbtn ws-plus"
                    aria-label="New session in {g.label}"
                    title="New session in {g.label}"
                    onclick={() => newSessionIn(g.path)}
                  >
                    <svg width="12" height="12" viewBox="0 0 12 12"><path d="M6 2.6v6.8M2.6 6h6.8" stroke="currentColor" stroke-width="1.4" fill="none"/></svg>
                  </button>
                {/if}
                <button
                  class="iconbtn more"
                  data-menu
                  aria-label="Folder actions"
                  onclick={(e) => openMenu('ws', e.currentTarget as HTMLElement, undefined, g.path)}
                >
                  <svg width="12" height="12" viewBox="0 0 12 12"><circle cx="2.6" cy="6" r="1.05" fill="currentColor"/><circle cx="6" cy="6" r="1.05" fill="currentColor"/><circle cx="9.4" cy="6" r="1.05" fill="currentColor"/></svg>
                </button>
              {/if}
            </div>
            {#if open}
              {#each g.list as s (s.id)}
                <div
                  class="sessrow {viewId === s.id ? 'active' : ''} {unread.has(s.id) ? 'unread' : ''}"
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
        {/each}
      {/if}
      </div>
    </aside>

    <!-- main -->
    <main class="flex min-w-0 flex-1 flex-col min-h-0" class:empty-mode={showEmpty}>
      {#if connNote}
        <!-- Rendered, not logged: a phone cannot open a console, and "no model"
             was sending people to fix a config file on the wrong machine. -->
        <div class="connbar" role="alert">
          <span class="connbar-txt">{connNote}</span>
          <button class="btn-ghost text-[11px]" onclick={() => void retryAttach()}>retry</button>
        </div>
      {/if}
      {#if showEmpty}
        <!-- Sibling of the composer, not a wrapper around it: `main` centres
             them as one group in empty mode. Spacer divs would absorb the free
             space above the composer and leave it pinned to the bottom, which
             is what they did. -->
        <div class="empty">
          <!-- The cup, not a dot. It is the one thing in this app that is not a
               rectangle, so it carries the warmth; the steam is the only bit of
               decorative motion in the app and it is tied to nothing. -->
          <div class="cupwrap" aria-hidden="true">
            <span class="steam s1"></span>
            <span class="steam s2"></span>
            <span class="steam s3"></span>
            <img class="cup" src="/icon.png" alt="" draggable="false" />
          </div>
          <h1 class="empty-hi">{greeting.hi}</h1>
          <p class="empty-sub">{greeting.sub}</p>
          {#if draftWsLabel}
            <!-- Saying which folder an unwritten session belongs to is the
                 difference between "a session in reports/" and a guess. Click it
                 to drop the preselection and go back to the default. -->
            <button class="empty-ws" title={draftWorkspace} onclick={() => (draftWorkspace = '')}>
              <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true"><path d="M1.5 3.2h3l1 1.3h5v4.8h-9z" fill="none" stroke="currentColor" stroke-width="1.1"/></svg>
              <span>in {draftWsLabel}</span>
              <span class="empty-ws-x" aria-hidden="true">✕</span>
            </button>
          {/if}
          <div class="tips" role="group" aria-label="Start with">
            {#each TIPS as t}
              <button class="tip" onclick={() => seedTip(t.text)}>{t.label}</button>
            {/each}
          </div>
        </div>
      {/if}
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
              <div class="attrow" role="list">
                {#each pending as a, i}
                  <span
                    class="attchip"
                    role="listitem"
                    aria-label="{a.name}, {fmtBytes(a.bytes)} - drag to reorder"
                    draggable="true"
                    class:dragging={dragIdx === i}
                    class:drop-target={dropIdx === i && dragIdx !== null && dragIdx !== i}
                    ondragstart={(e: DragEvent) => {
                      dragIdx = i
                      e.dataTransfer?.setData('text/plain', String(i))
                      if (e.dataTransfer) e.dataTransfer.effectAllowed = 'move'
                    }}
                    ondragend={() => { dragIdx = null; dropIdx = null }}
                    ondragover={(e: DragEvent) => {
                      if (dragIdx === null) return
                      e.preventDefault()
                      e.stopPropagation()
                      dropIdx = i
                      if (e.dataTransfer) e.dataTransfer.dropEffect = 'move'
                    }}
                    ondrop={(e: DragEvent) => {
                      if (dragIdx === null) return
                      // The composer card also has a drop handler for incoming
                      // files; without this a reorder would try to attach the
                      // dragged chip as a new attachment.
                      e.preventDefault()
                      e.stopPropagation()
                      moveAttachment(dragIdx, i)
                      dragIdx = null
                      dropIdx = null
                    }}
                  >
                    <img src={a.url} alt="" />
                    <span class="att-meta">
                      <span class="att-name">{a.name}</span>
                      <span class="att-size">{fmtBytes(a.bytes)}</span>
                    </span>
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

          {#if wsEditing}
            <div class="wsrow ws-edit">
              <svg width="13" height="13" viewBox="0 0 14 14"><path d="M1.8 4.2h3.4l1 1.4h6v5.6a.8.8 0 0 1-.8.8H2.6a.8.8 0 0 1-.8-.8z" fill="none" stroke="currentColor" stroke-width="1.1"/></svg>
              <input
                class="ws-input"
                bind:value={wsDraft}
                use:focusNow
                spellcheck="false"
                aria-label="Host workspace path"
                placeholder="/path/on/the/host"
                onkeydown={(e) => {
                  if (e.key === 'Enter') {
                    e.preventDefault()
                    void applyWorkspace(wsDraft.trim())
                  } else if (e.key === 'Escape') {
                    wsEditing = false
                    wsErr = ''
                  }
                }}
              />
              <button class="ws-act" onclick={() => void applyWorkspace(wsDraft.trim())}>set</button>
              <button class="ws-act" onclick={() => { wsEditing = false; wsErr = '' }}>cancel</button>
            </div>
          {:else}
            <button
              class="wsrow"
              title={isTauri()
                ? 'Choose the directory the tools operate in'
                : `Host workspace: ${hostState?.workspace ?? ''} - click to type a path`}
              onclick={chooseWorkspace}
            >
              <svg width="13" height="13" viewBox="0 0 14 14"><path d="M1.8 4.2h3.4l1 1.4h6v5.6a.8.8 0 0 1-.8.8H2.6a.8.8 0 0 1-.8-.8z" fill="none" stroke="currentColor" stroke-width="1.1"/></svg>
              <span>{shortWorkspace || 'Choose workspace'}</span>
              {#if wsBusy}<span class="ws-note">working…</span>{/if}
            </button>
          {/if}
          {#if wsErr}<div class="ws-err" role="alert">{wsErr}</div>{/if}
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
      class="pop {menuPos ? '' : 'measuring'} {menu.kind === 'metrics' ? 'pop-wide' : ''}"
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
          {#each models as m (m.id)}
            <button
              class="pop-item pop-model {model === m.id ? 'on' : ''}"
              onclick={() => pickModel(m.id)}
              role="menuitemradio"
              aria-checked={model === m.id}
            >
              <span class="pop-lbl pop-mono">{m.id}</span>
              <span class="mchips">
                {#if m.context_tokens}
                  <span class="mchip" title="Context window reported by the endpoint">{fmtTokens(m.context_tokens)} ctx</span>
                {/if}
                {#if visionOf(m) !== null}
                  <span class="mchip {visionOf(m) ? 'yes' : 'dim'}" title="Input modalities reported by the endpoint: {m.input_modalities.join(', ')}">
                    {visionOf(m) ? 'vision' : 'text only'}
                  </span>
                {/if}
                {#if m.tools !== null}
                  <span
                    class="mchip {m.tools ? 'yes' : 'dim'}"
                    title={m.tools_inferred
                      ? 'Detected from the chat template this server serves; the endpoint does not state tool support outright'
                      : 'Stated by the endpoint'}
                  >{m.tools ? 'tool calling' : 'no tools'}{#if m.tools_inferred}<span class="mchip-mark">*</span>{/if}</span>
                {/if}
                {#if m.slots}
                  <span class="mchip" title="Concurrent slots the endpoint advertises">{m.slots} slot{m.slots === 1 ? '' : 's'}</span>
                {/if}
                {#if !m.advertised}
                  <span class="mchip dim" title="The endpoint published no capabilities for this model">not reported</span>
                {/if}
              </span>
              {#if m.source || m.server}
                <span class="msub pop-mono" title={`${m.source ?? ''} ${m.server ?? ''}`.trim()}>
                  {#if m.source}{m.source}{/if}{#if m.server}{#if m.source}{' \u00b7 '}{/if}{m.server}{/if}
                </span>
              {/if}
            </button>
          {/each}
          {#if models.some((m) => m.tools_inferred)}
            <div class="pop-note">* detected from the served chat template; the endpoint does not state it.</div>
          {/if}
        {:else}
          <div class="pop-empty">
            {#if !ipc.ok}
              <!-- Not the endpoint's fault: there is no connection to ask. Saying
                   "this endpoint does not list models" here sends people to debug
                   a server that is running fine. -->
              <strong>Not connected to the host.</strong><br />
              <span class="pop-mono">{ipc.msg}</span>
            {:else if modelErr}
              <strong>Cannot reach the endpoint.</strong><br />
              <span class="pop-mono">{modelErr}</span><br />
              Start the server, or fix the URL in
              <span class="pop-mono">{transportKind === 'gateway' ? 'the host\'s ~/.config/cwoffee/config.toml' : '~/.config/cwoffee/config.toml'}</span>.
              {#if transportKind === 'gateway'}<br />This page talks to the desktop's engine; it has no endpoint of its own.{/if}
            {:else}
              This endpoint does not list models.<br />
              Using <span class="pop-mono">{model || '\u2014'}</span>
            {/if}
          </div>
        {/if}
      {:else if menu.kind === 'ws'}
        {@const wp = menu.wsPath ?? ''}
        {@const wrow = workspaces.find((w) => w.path === wp)}
        {@const wlabel = wrow ? wsLabelOf(wrow) : wp ? basenameOf(wp) : 'No workspace'}
        <div class="pop-title">{wlabel}</div>
        <div class="pop-mono pop-sub">{wp || 'sessions with no folder'}</div>
        {#if wp}
          <button
            class="pop-item"
            role="menuitem"
            onclick={() => newSessionIn(wp)}
          >
            <span class="pop-lbl">New session here</span>
            <span class="pop-sub">Opens the greeting with this folder selected</span>
          </button>
          <button
            class="pop-item"
            role="menuitem"
            onclick={() => startWsRename(wp, wrow?.label ?? '')}
          >
            <span class="pop-lbl">Rename</span>
            <span class="pop-sub">Changes the name in the list, never the folder</span>
          </button>
          {#if gatewayRole() !== 'watch'}
            <button
              class="pop-item danger"
              role="menuitem"
              onclick={() => void removeFolder(wp)}
            >
              <span class="pop-lbl">Remove from sidebar</span>
              <span class="pop-sub">
                {sessions.filter((x) => x.workspace === wp).length
                  ? 'Sessions that live here stay, so the folder comes back'
                  : 'The folder has no sessions in it'}
              </span>
            </button>
          {/if}
        {/if}
      {:else if menu.kind === 'metrics'}
        <div class="pop-title">Session telemetry</div>
        {#if !lastUsage && !turnMs}
          <!-- Nothing measured yet. A wall of em-dashes reads as broken
               instrumentation; this says what is actually true. -->
          <div class="pop-empty">
            Nothing measured in this window yet. Numbers appear while the first
            turn streams and stay for the rest of the session.
          </div>
        {/if}
        <div class="mbar-row">
          <span class="mlbl">context</span>
          <span class="mbar" title="prompt tokens against the ceiling the endpoint states">
            <i style="width:{Math.min(100, ctxPct)}%"></i>
          </span>
          <span class="mval mono">
            {(hostState?.context_tokens ?? 0).toLocaleString()} /
            {ctxLimit ? fmtTokens(ctxLimit) : 'not reported'}
          </span>
        </div>
        <div class="mbar-row">
          <span class="mlbl">prefix cache</span>
          <span class="mbar" title="how much of the prompt the server already had">
            <i class="ok" style="width:{Math.min(100, cacheHit)}%"></i>
          </span>
          <span class="mval mono">{cacheLabel} ({cachedTokens}/{promptTokens})</span>
        </div>
        <div class="mgrid">
          <div class="mcell">
            <b>{liveEstimate ? '\u2248 ' : ''}{fmt.rate(curDecode)}</b>
            <span>decode/s now{liveEstimate ? ' · estimated' : ''}</span>
          </div>
          <div class="mcell"><b>{fmt.rate(avgDecode)}</b><span>decode/s session avg</span></div>
          <div class="mcell"><b>{fmt.rate(curPrefill)}</b><span>prefill/s effective</span></div>
          <div class="mcell"><b>{fmt.rate(avgPrefill)}</b><span>prefill/s raw avg</span></div>
          <div class="mcell">
            <b>{ttft || '\u2014'}<small>ms</small></b><span>first token</span>
          </div>
          <div class="mcell">
            <b>{turnMs ? (turnMs / 1000).toFixed(1) : '\u2014'}<small>s</small></b>
            <span>this turn</span>
          </div>
          <div class="mcell"><b>{fmt.pct(specPct)}</b><span>speculative accept</span></div>
          <div class="mcell"><b>{lastUsage?.completion ?? 0}</b><span>tokens out</span></div>
          <div class="mcell">
            <b>{lastUsage?.prompt ?? promptTokens ?? 0}</b><span>tokens in (prompt)</span>
          </div>
          <div class="mcell">
            <b>{hostState?.compactions ?? 0}</b>
            <span title="history was dropped; the next prefill will be slow">compactions</span>
          </div>
        </div>
        <div class="mfoot">
          <span class="mfoot-lbl">render</span>
          <span class="mfoot-v mono">
            {frames} frames · {dropped} dropped · worst {worstFrame.toFixed(0)}ms
          </span>
        </div>
        {#if transportKind === 'tauri'}
          <div class="mfoot">
            <span class="mfoot-lbl">host</span>
            <span class="mfoot-v mono">
              ipc {ipc.ok ? 'ok' : ipc.msg} ·
              {gpu
                ? gpuOk
                  ? `iGPU ${gpuNodes.join(',')}`
                  : `GPU LEAK ${gpuNodes.join(',')}`
                : 'gpu not reported'}
            </span>
          </div>
        {/if}
        <div class="pop-note">
          Rates marked ≈ are estimated from characters: the server reports token
          counts only when a turn ends. Context is the ceiling the endpoint
          states, not a reservation.
        </div>
      {:else if menu.kind === 'gateway'}
        <div class="pop-title">Remote access</div>
        {#if transportKind !== 'tauri'}
          <div class="pop-item static">
            <span class="pop-lbl">You are on it</span>
            <span class="pop-sub">
              This window is served by the gateway running on the host machine.
              Turn it off from the desktop app on that machine.
            </span>
          </div>
          <div class="pop-note">
            Plain http, no tls: anyone who can reach this address and read the
            link can drive this agent, workspace and tools included.
          </div>
        {:else}
          {@const on = !!gw?.running}
          <button
            class="pop-item gwsw"
            role="menuitemcheckbox"
            aria-checked={on}
            disabled={gwBusy}
            onclick={() => void setGateway(!on)}
          >
            <span class="pop-lbl">
              {gwBusy ? 'changing\u2026' : on ? 'Turn off remote access' : 'Turn on remote access'}
            </span>
            <span class="switch {on ? 'on' : ''}" aria-hidden="true"><i></i></span>
          </button>
          <div class="pop-item static">
            <span class="pop-lbl">Reachable from</span>
            <span class="pop-sub">
              {on
                ? `bound to ${gw?.host}:${gw?.port}`
                : gwScope === 'lan'
                  ? 'next start: every interface (0.0.0.0)'
                  : 'next start: this machine only'}
            </span>
          </div>
          <div class="popseg">
            <button
              class="pop-item {gwScope === 'loopback' ? 'on' : ''}"
              role="menuitemradio"
              aria-checked={gwScope === 'loopback'}
              onclick={() => void setScope('loopback')}
            >
              <span class="pop-lbl">This machine</span>
              <span class="pop-sub">127.0.0.1</span>
            </button>
            <button
              class="pop-item {gwScope === 'lan' ? 'on' : ''}"
              role="menuitemradio"
              aria-checked={gwScope === 'lan'}
              onclick={() => void setScope('lan')}
            >
              <span class="pop-lbl">Any device</span>
              <span class="pop-sub">0.0.0.0 · needs a token</span>
            </button>
          </div>
          {#if on}
            <div class="pop-item static">
              <span class="pop-lbl">Attached now</span>
              <span class="pop-sub mono">
                {attachedLine(gw)} · token from {gw?.token_source ?? 'none'}
              </span>
            </div>
            {#each gw?.urls ?? [] as u}
              <button class="pop-item urlrow" onclick={() => void copyUrl(u)} role="menuitem">
                <span class="pop-lbl mono url">{u}</span>
                <span class="pop-sub">{copiedUrl || 'copy'}</span>
              </button>
            {/each}
          {/if}
          {#if gwErr}
            <div class="ws-err">{gwErr}</div>
          {/if}
          <div class="pop-note">
            Plain http, no tls. A drive token can send turns, move the workspace
            and run sandboxed commands, so the link is the credential: anyone
            who reads it can drive this agent.
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
          <button
            class="pop-item danger"
            onclick={() => {
              confirmDeleteId = sid
              // Let the prompt expire rather than hanging open forever.
              setTimeout(() => {
                if (confirmDeleteId === sid) confirmDeleteId = null
              }, 4000)
            }}
            role="menuitem"
          >
            Delete
          </button>
        {/if}
      {/if}
    </div>
  {/if}

  <!-- ========================== telemetry bar ========================= -->
  <footer class="telem flex h-9 shrink-0 items-center gap-2 border-t border-hairline bg-paper-2 px-3 text-[11.5px] text-ink-2">
    <!-- Two controls instead of eight readouts. Every number is still there, one
         click away, and the space they occupied now carries the thing that is
         actually switched during a day. -->
    <button
      class="fbtn"
      data-menu
      aria-haspopup="menu"
      aria-expanded={menu?.kind === 'metrics'}
      aria-label="Session telemetry"
      onclick={(e) => openMenu('metrics', e.currentTarget as HTMLElement)}
    >
      <span class="fdot {dropped > 0 ? 'hot' : ''}" aria-hidden="true"></span>
      <span class="flbl">metrics</span>
      <span class="fsum">
        <b>{cacheLabel}</b>
        <span class="fsub">cache</span>
        <span class="fsep">·</span>
        <b>{liveEstimate ? '\u2248' : ''}{fmt.rate(curDecode)}</b>
        <span class="fsub">tok/s</span>
        {#if ctxLimit}
          <span class="fsep">·</span>
          <b>{ctxLabel}</b>
          <span class="fsub">ctx</span>
        {/if}
      </span>
    </button>

    <button
      class="fbtn"
      data-menu
      aria-haspopup="menu"
      aria-expanded={menu?.kind === 'gateway'}
      aria-label="Remote access"
      onclick={(e) => openMenu('gateway', e.currentTarget as HTMLElement)}
    >
      <span class="fdot {gwDot}" aria-hidden="true"></span>
      <span class="flbl">gateway</span>
      <span class="fsum">{gwText}</span>
    </button>

    <span class="ml-auto flex items-center gap-2">
      <span
        class="rounded-full px-2 py-0.5 {ipc.ok ? 'bg-paper-2 text-ink-3' : 'bg-danger-bg text-danger'}"
        title="Tauri IPC round-trip: {ipc.msg}"
      >
        ipc {ipc.ok ? '\u2713' : '\u2717'}
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
