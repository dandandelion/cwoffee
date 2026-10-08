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
    QuestionRequested,
    ReasoningLevel,
    SearchHit,
    Session,
    StoredMessage,
    WorkspaceRow,
  } from './lib/events'
  import { Rolling, statsFromTurn, fmt } from './lib/live'
  import { Transcript, linkProbe, subjectFor, riskHeadline } from './lib/transcript'
  import { installContextMenu } from './lib/ctxmenu'
  import { pageLog } from './lib/pagelog'

  // ---- state --------------------------------------------------------------
  let rootEl: HTMLDivElement | undefined = $state()
  let transcript: Transcript | null = null

  let busy = $state(false)
  let model = $state('')
  let draft = $state('')
  let mode = $state<'ask' | 'session' | 'auto'>('ask')
  let thinking = $state<ReasoningLevel>('xhigh')
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

  /**
   * A one-line note that leaves on its own. Refusals used to land only in the
   * inline error slot of whichever panel raised them - so a folder rejected by the
   * engine could be refused with the panel already closed, and the only trace was a
   * line in the server log. Anything the user asked for and did not get says so here.
   */
  let toast = $state<{ text: string; bad: boolean } | null>(null)
  let toastTimer = 0
  /** Read the current toast text. Reading `toast` directly inside a long async
      block lets TS narrow the $state to `null` after an assignment and then call
      the next read `never`. */
  function toastText(): string {
    return (toast as { text: string } | null)?.text ?? ''
  }

  function say(text: string, bad = false) {
    toast = { text, bad }
    if (toastTimer) clearTimeout(toastTimer)
    toastTimer = setTimeout(() => (toast = null), 4600)
  }

  let query = $state('')
  let hits = $state<SearchHit[]>([])
  let searchErr = $state('')

  type ApprovalPrompt = { id: number; tool: string; risk: string; summary: string }
  let approval = $state<ApprovalPrompt | null>(null)
  // The quoted command scrolls inside the card; the buttons do not move. The
  // hint only appears when the text really is clipped, measured after render.
  let apprBodyEl = $state<HTMLElement | null>(null)
  let apprClipped = $state(false)
  $effect(() => {
    if (!approval) {
      if (apprClipped) apprClipped = false
      return
    }
    const el = apprBodyEl
    if (!el) return
    const clipped = el.scrollHeight > el.clientHeight + 4
    if (clipped !== apprClipped) apprClipped = clipped
  })
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
  type MenuKind = 'mode' | 'model' | 'think' | 'row' | 'keys' | 'metrics' | 'gateway' | 'ws' | 'search'

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
  const THINK_LABEL: Record<ReasoningLevel, string> = {
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

  /**
   * The system temp folder is not a workspace, it is where sessions go before
   * anyone chooses a folder. Half the old sessions had `/tmp` written into them by
   * an early default, and the sidebar drew it as a folder group called `tmp` next
   * to a group called `No workspace`: the same thing, in two rows, neither of them
   * the truth. Anything in this set buckets as "no workspace", and the engine
   * refuses to register it as a folder you work in.
   */
  const TEMP_ROOTS = ['/tmp', '/var/tmp', '/private/tmp']
  function isTempRoot(p: string | null | undefined): boolean {
    const q = (p ?? '').trim().replace(/[\\/]+$/, '')
    if (!q) return true
    return TEMP_ROOTS.some((t) => q.toLowerCase() === t)
  }
  /** Sidebar bucket for a session: the folder, or "" for "no workspace". */
  function wsBucket(p: string | null | undefined): string {
    return isTempRoot(p) ? '' : (p as string)
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
      const k = wsBucket(s.workspace)
      const arr = byPath.get(k) ?? []
      arr.push(s)
      byPath.set(k, arr)
    }
    const out: WsGroup[] = []
    const seen = new Set<string>()
    for (const w of workspaces) {
      // A temp root on the registered list is history, not a folder: an older
      // build used to write it as the default. Its sessions are already in the
      // Default bucket.
      if (isTempRoot(w.path)) continue
      out.push({ path: w.path, label: wsLabelOf(w), list: byPath.get(w.path) ?? [] })
      seen.add(w.path)
    }
    // A session in a folder that is not on the list still has to be reachable.
    // Hiding work because the sidebar had no row for it is the failure mode.
    for (const [path, list] of byPath) {
      if (seen.has(path)) continue
      out.push({ path, label: path ? basenameOf(path) : 'Default', list })
      seen.add(path)
    }
    return out
  })

  /**
   * Whether the folder picker in the composer should be closed for business.
   *
   * A session picks its folder when it starts and keeps it. Moving one mid-
   * conversation makes the sidebar grouping decoration - the row would still say
   * `code/app` while `write_file` landed somewhere else - and it can quietly write
   * into a tree nobody chose. So once a session has a real folder the control is
   * disabled rather than merely discouraged, and the engine refuses the same request
   * whatever client sends it.
   */
  const wsLocked = $derived.by(() => {
    // A draft has no folder of its own yet: the folder it picks becomes the folder
    // of the session it turns into. Only an open session is locked to its folder.
    if (viewId === null) return false
    const stored = sessions.find((s) => s.id === viewId)?.workspace ?? ''
    return !!stored && !isTempRoot(stored)
  })

  /** What the composer's folder chip says. In a draft it names the folder the
   * session will get, which need not be the folder the host is still holding. */
  // The folder shown next to the composer is the folder of the conversation on
  // screen, not whatever the engine last moved to: since browsing no longer moves
  // the engine, the two legitimately differ.
  const wsChipLabel = $derived.by(() => {
    if (viewId === null) return draftWorkspace ? draftWsLabel : shortWorkspace
    const stored = sessions.find((s) => s.id === viewId)?.workspace ?? ''
    return stored ? basenameOf(stored) : 'no folder'
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
      say(`added ${basenameOf(added)} to the sidebar`)
    } catch (e) {
      // The refusal comes from the engine, which is the only thing that knows
      // whether a path is a directory, writable, or the temp folder. Repeating that
      // judgement here would be a second opinion nobody asked for.
      wsAddErr = cleanErr(e)
      say(`not added: ${wsAddErr}`, true)
      pageLog(`add_workspace failed: ${String(e)}`)
    }
  }

  /**
   * The `+` in the Workspaces header. On the desktop it opens the native folder
   * picker straight away - a panel that asks you to type a path before it will open
   * a picker that picks it for you is one step that should not exist. In the browser
   * there is no picker to open (the folder is on the host, not on the phone), so the
   * typed path stays as the fallback there.
   */
  function addWorkspaceFromHeader() {
    wsAddErr = ''
    if (transportKind === 'tauri') {
      wsAdding = false
      wsAddDraft = ''
      void addFolderPicked()
      return
    }
    wsAdding = !wsAdding
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
      if (!picked) {
        // "nothing" is the normal outcome of pressing Cancel, and it used to be
        // silent: the panel sat there with no explanation for why nothing happened.
        say('no folder chosen')
        return
      }
      await addFolderFor(picked)
    } catch (e) {
      wsAddErr = cleanErr(e)
      say(`not added: ${wsAddErr}`, true)
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
    // Search settings are read on first open rather than at boot: nothing here is
    // needed to start a turn, and a container status probe (podman ps) has no
    // business running before anyone asked about search.
    if (kind === 'search' && !webSearch) void loadWebSearch()
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
  async function pickThinking(t: ReasoningLevel) {
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

  // ---- web search provider ------------------------------------------------
  // Three providers, one dropdown, and a test button. The status line comes from
  // the engine rather than from the value in this component: "auto" with no
  // container runtime is not the same situation as "auto" with one, and a card
  // that prints the config value cannot tell them apart.
  interface SearchAuto {
    up: boolean
    container_exists: boolean
    runtime: string | null
    base: string
    note: string
  }
  interface SearchView {
    backend: string
    searxng_url: string
    auto_port: number
    auto_idle_secs: number
    auto_dir: string
    describing: string
    runtime: string | null
    auto: SearchAuto | null
    env_override: string | null
    saved_to: string | null
  }
  interface WebSearchProbe {
    ms: number
    note: string | null
    backend: string
    results: { title: string; url: string }[]
    error: string | null
  }
  let webSearch = $state<SearchView | null>(null)
  let webSearchErr = $state('')
  let webSearchBusy = $state(false)
  let webSearchProbe = $state<WebSearchProbe | null>(null)
  let webSearchUrl = $state('')

  const SEARCH_PROVIDERS: { id: string; label: string; sub: string }[] = [
    {
      id: 'duckduckgo',
      label: 'DuckDuckGo',
      sub: 'no setup, no key · starts blocking you after a few searches',
    },
    {
      id: 'auto',
      label: 'SearXNG, run by Cwoffee',
      sub: 'its own container, started when you search, stopped when idle',
    },
    { id: 'searxng', label: 'SearXNG, already running', sub: 'point at an instance you run' },
  ]

  const webSearchText = $derived(
    !webSearch
      ? 'unknown'
      : webSearch.backend === 'auto'
        ? webSearch.auto?.up
          ? `searxng :${webSearch.auto_port} up`
          : webSearch.runtime
            ? `searxng :${webSearch.auto_port} idle`
            : 'no container runtime'
        : webSearch.backend === 'searxng'
          ? 'searxng (manual)'
          : 'duckduckgo',
  )
  const webSearchDot = $derived(
    webSearch?.backend === 'duckduckgo' ? 'warn' : webSearch?.backend ? 'ok' : '',
  )

  async function loadWebSearch() {
    try {
      const v = await invoke<SearchView>('search_settings')
      webSearch = v
      webSearchUrl = v.searxng_url ?? ''
      webSearchErr = ''
    } catch (e) {
      webSearchErr = String(e)
    }
  }

  async function chooseWebSearch(id: string) {
    if (!webSearch || webSearchBusy) return
    webSearchBusy = true
    webSearchErr = ''
    webSearchProbe = null
    try {
      webSearch = await invoke<SearchView>('set_search_backend', {
        backend: id,
        searxngUrl: webSearchUrl,
      })
      say(`search: ${webSearch.describing}`)
    } catch (e) {
      // The engine refused (no URL for a manual instance, a typo in the name).
      // Say why in place rather than leaving the dropdown on the old value with no
      // explanation.
      webSearchErr = String(e)
    } finally {
      webSearchBusy = false
    }
  }

  async function runWebSearchProbe() {
    if (webSearchBusy) return
    webSearchBusy = true
    webSearchErr = ''
    try {
      webSearchProbe = await invoke<WebSearchProbe>('search_probe', { query: 'coffee roasting' })
      if (webSearchProbe.error) webSearchErr = webSearchProbe.error
    } catch (e) {
      webSearchErr = String(e)
    } finally {
      webSearchBusy = false
    }
  }
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
          : // "off" is true but useless when the reason is a port someone else
            // holds; the reason is the whole message.
            gw.note && gw.note !== 'off'
            ? gw.note
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
  // The Cowork/Chat tab is a stated direction, not a feature: nothing is wired to
  // it, and it is kept visibly inert rather than half-implemented.
  let modeTab = $state<'cowork' | 'chat'>('cowork')
  /**
   * Sessions with a turn in flight, for the sidebar. EngineEvent::TurnStart carries
   * no session id, so this is the session that was active when the turn began -
   * which is the same thing, and it survives the view moving on to another one.
   */
  let runningSessions = $state<Set<number>>(new Set())
  /**
   * How many turnEnd events this webview has seen, for the harness only. HostState's
   * `turns` is per session, so a check that compares it across a session switch waits
   * on a counter that belongs to a different conversation - which is how a passing
   * suite started reporting "turn never ended" for turns that had plainly ended.
   */
  let turnEnds = 0
  // The endpoint's own prefill rate from the last completed turn.
  let lastPrefill: number | null = null
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

  // A question set is one `ask_user` call, delivered as one event per question.
  // Buffered by set id so the card can be built once, with navigation, instead of
  // one interruption per question.
  const qSets = new Map<number, { qs: Map<number, QuestionRequested>; timer: number }>()

  function respondQuestion(id: number, r: { text: string; options: number[] }) {
    void invoke('respond_question', { id, text: r.text, options: r.options }).catch((e) => {
      pageLog(`respond_question failed: ${cleanErr(String(e))}`)
      // The engine stopped waiting before we answered. Close the card the same way
      // the expiry event would, rather than leaving a button that fails every time
      // it is pressed.
      transcript?.expireQuestion(id)
    })
  }

  function renderQuestionSet(setId: number) {
    const b = qSets.get(setId)
    if (!b) return
    qSets.delete(setId)
    if (b.timer) window.clearTimeout(b.timer)
    const qs = [...b.qs.values()]
    if (!qs.length) return
    if (qs.length === 1) {
      // One question is one question, whatever the model called it. Render it with
      // the plain card so the simpler shape keeps its simpler layout.
      const q = qs[0]
      transcript?.showQuestion(q, (r) => respondQuestion(q.id, r))
      return
    }
    pageLog(`question set rendered: ${qs.length} questions on one card`)
    transcript?.showQuestionSet(qs, (id, r) => respondQuestion(id, r))
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
        if (turnSessionId !== null) {
          const next = new Set(runningSessions)
          next.add(turnSessionId)
          runningSessions = next
        }
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
      case 'questionRequested': {
        // The turn is parked until this is answered, so the answer goes straight
        // back; nothing else in the UI needs to know.
        const q = ev.d
        pageLog(`question asked: ${q.question}`)
        if (!q.set) {
          transcript?.showQuestion(q, (r) => respondQuestion(q.id, r))
          break
        }
        // A set arrives as separate events, back to back. Rendering each as it lands
        // would give one card per question, which is the interruption the set form
        // exists to remove, so buffer by set id until the whole set is here. If an
        // event is lost - a reconnect mid-turn, say - a short timer renders what did
        // arrive rather than parking the turn behind a card that never shows up.
        const b = qSets.get(q.set.id) ?? { qs: new Map<number, QuestionRequested>(), timer: 0 }
        b.qs.set(q.id, q)
        if (!b.timer) b.timer = window.setTimeout(() => renderQuestionSet(q.set!.id), 400)
        qSets.set(q.set.id, b)
        if (b.qs.size >= q.set.total) renderQuestionSet(q.set.id)
        break
      }
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
        // Kept for the self-test: a turn that does not finish can be a hang or a
        // model prefilling at 17 tok/s, and the difference decides who is buggy.
        lastPrefill = s.effectivePrefillTps
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
        turnEnds++
        if (turnSessionId !== null && runningSessions.delete(turnSessionId)) {
          runningSessions = new Set(runningSessions)
        }
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
        // Most engine errors are notes the transcript already shows. Three are
        // decisions about the user's own action, and they have to be visible even
        // when the transcript is scrolled away: the turn did not run, the attachment
        // was rejected, or the folder was refused.
        else if (/nothing was sent|rejected|the engine refused/i.test(ev.d.message)) {
          say(ev.d.message, true)
        }
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
    if (wsLocked) {
      // A dead control should say why it is dead. The engine refuses the same
      // request over the gateway; here it is explained before the click.
      say(
        `this session works in ${shortWorkspace} - a session keeps its folder, so start a new one to work somewhere else`,
      )
      return
    }
    // A phone cannot open a folder picker on a machine it is not sitting at, so
    // in gateway mode the control becomes a path field instead of doing nothing.
    if (transportKind !== 'tauri') {
      wsDraft = (viewId === null ? draftWorkspace : hostState?.workspace) ?? ''
      wsEditing = true
      return
    }
    wsBusy = true
    try {
      const picked = (await invoke('pick_workspace')) as string | null
      if (!picked) return
      // In a draft the folder belongs to the session about to be created, not to
      // the session the host is still holding. Sending set_workspace here would ask
      // the engine to move a session that already has a folder - and it refuses,
      // correctly, so the one place the control should work would fail.
      if (viewId === null) applyDraftWorkspace(picked)
      else await applyWorkspace(picked)
    } catch (e) {
      wsErr = cleanErr(e)
      pageLog(`pick_workspace failed: ${String(e)}`)
    } finally {
      wsBusy = false
    }
  }

  /** The folder a not-yet-created session will use. No RPC: `new_session` carries
   * it, and the engine validates it there. */
  function applyDraftWorkspace(path: string) {
    if (!path) return
    draftWorkspace = path
    wsEditing = false
    wsDraft = ''
    wsErr = ''
    say(`a new session will work in ${basenameOf(path)}`)
  }

  /** Submit the typed-path field: the target depends on whether a session is open. */
  function submitWsPath() {
    const path = wsDraft.trim()
    if (!path) return
    if (viewId === null) applyDraftWorkspace(path)
    else void applyWorkspace(path)
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

  /**
   * Make the host idle before the next check touches it: wait a bounded time, then
   * do what a user would and stop the turn. Without this one slow turn cascades -
   * every later step reports the *previous* turn ("a turn is running, so the
   * workspace change is queued behind it", "cannot delete the active session while a
   * turn is running") and the failure names the wrong step.
   */
  async function settleTurn(what: string, budgetMs = 60000): Promise<boolean> {
    const idle = async () => !(await invoke<HostState>('session_state')).busy
    // Idle has to *persist* before it counts. A turn queued behind the one just
    // cancelled starts a moment later, so returning on the first idle sample hands the
    // next check a busy host, and it fails on a message about a previous step.
    const idleHolds = async () => {
      for (let i = 0; i < 6; i++) {
        if (!(await idle())) return false
        await new Promise((r) => setTimeout(r, 250))
      }
      return true
    }
    const deadline = performance.now() + budgetMs
    while (performance.now() < deadline) {
      if (await idleHolds()) return true
      await new Promise((r) => setTimeout(r, 250))
    }
    // Cancel more than once. A turn queued behind the one being cancelled starts as
    // soon as the first stops, so a single cancel can leave the host busy again and
    // the harness concludes the engine is wedged when it is only behind.
    for (let attempt = 1; attempt <= 3; attempt++) {
      await invoke('cancel_turn').catch(() => {})
      // The host goes idle as soon as it drops the request, but the endpoint keeps
      // working on the aborted one; the next turn then sits in its queue and looks
      // like a turn that never started.
      await new Promise((r) => setTimeout(r, 2500))
      const until = performance.now() + 15000
      while (performance.now() < until) {
        if (await idleHolds()) {
          pageLog(`settleTurn: ${what} was cancelled (attempt ${attempt}) instead of waited out`)
          return true
        }
        await new Promise((r) => setTimeout(r, 250))
      }
    }
    pageLog(`settleTurn: ${what} is still busy after 3 cancels`)
    return false
  }

  async function waitTurnDone(prevTurns: number, budgetMs = 240000): Promise<boolean> {
    const endsBefore = turnEnds
    const dl = performance.now() + budgetMs
    let lastTurns = prevTurns
    while (performance.now() < dl) {
      // The event is the truth: this client saw the turn finish. The store counter is
      // kept as a second route for a turn that ended while the page was reconnecting.
      if (turnEnds > endsBefore) return true
      lastTurns = (await invoke<HostState>('session_state')).turns
      if (lastTurns > prevTurns) return true
      await new Promise((r) => setTimeout(r, 200))
    }
    // Say what was waited for and for how long. Two checks used to fail here with
    // "turn never finished" and no way to tell a hang from a slow model; the
    // endpoint serves one slot and a 60s first token has been measured under
    // contention, so the budget has to sit above that and the timeout has to name
    // itself when it still trips.
    // 240s is not taste either: a cold prefill of ~2k tokens on this box has been
    // measured at 12.8 tok/s, i.e. a 153s first token, and the turn completed
    // correctly right after the old 150s window closed. A harness that times out
    // under the model invents bugs.
    pageLog(
      `waitTurnDone: no turnEnd after ${budgetMs}ms (turns=${prevTurns}->${lastTurns}). ` +
        'If telemetry shows prefill under ~50 tok/s this is the endpoint being slow, not the app stuck.',
    )
    return false
  }

  async function turnCount(): Promise<number> {
    return (await invoke<HostState>('session_state')).turns
  }

  /**
   * Sample the page's own frame clock and return its statistics.
   *
   * This is the heartbeat of every animation in the window: if the engine stops
   * handing out frames, nothing here can look smooth no matter how the CSS is
   * written. It is deliberately *not* a blink detector - a frame dropped after
   * the engine painted it (which is what the DMABUF artifact was) is invisible
   * to rAF. What it does catch is cost: a rendering path that spends the frame
   * budget on copies or repaints shows up as widening gaps while an answer
   * streams, which is the price of `CWOFFEE_WEBKIT_MODE` and has to be measured
   * rather than assumed free.
   */
  /** Sample the frame clock while nothing is streaming: the machine's own ceiling. */
  async function idleFrameSample(ms = 900) {
    const stop = startFrameCadence()
    await new Promise((r) => setTimeout(r, ms))
    return stop()
  }

  function startFrameCadence() {
    const gaps: number[] = []
    // A long task is the page's own work blocking the frame clock - 50ms or more in
    // one task. Counting them is what lets this check tell "our JS stalled the
    // page" from "the process was not given a frame": the first is a bug in here,
    // the second is the machine, and on a box serving a 50 GB model they look
    // identical in the gap histogram alone.
    let longN = 0
    let longMs = 0
    let obs: PerformanceObserver | null = null
    try {
      obs = new PerformanceObserver((list) => {
        for (const e of list.getEntries()) {
          longN++
          longMs += e.duration
        }
      })
      obs.observe({ entryTypes: ['longtask'] })
    } catch {
      /* no long-task support: the attribution then says so rather than claiming 0 */
    }
    let last = performance.now()
    let id = 0
    const t0 = last
    let hiddenSpans = 0
    let hiddenPrev = document.hidden
    let worst = { ms: 0, at: 0 }
    const tick = (t: number) => {
      const gap = t - last
      // A page that is not on screen is not animated, and WebKitGTK stops the
      // frame loop when its surface is occluded. Counting that as a hitch would
      // make this check measure window management instead of rendering, so the
      // resume gap after a hidden span is dropped - and the count of hidden spans
      // is reported, so a sample taken off screen is visible as one.
      if (!hiddenPrev) {
        gaps.push(gap)
        if (gap > worst.ms) worst = { ms: gap, at: t - t0 }
      }
      const hiddenNow = document.hidden
      if (hiddenNow && !hiddenPrev) hiddenSpans++
      hiddenPrev = hiddenNow
      last = t
      id = requestAnimationFrame(tick)
    }
    id = requestAnimationFrame(tick)
    return () => {
      cancelAnimationFrame(id)
      obs?.disconnect()
      const sorted = gaps.slice(1).sort((a, b) => a - b)
      const at = (q: number) => sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * q))] ?? 0
      return {
        frames: gaps.length,
        ms: Math.round(last - t0),
        median: at(0.5),
        p95: at(0.95),
        worst: worst.ms,
        worstAt: worst.at,
        hiddenSpans,
        longN,
        longMs,
        longTasksMeasured: obs !== null,
      }
    }
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
    // The engine files a turn in the conversation it has active and refuses one
    // addressed to another. If another client moved the host while this window sat
    // on session X, "send" still means "into what I am looking at" - so move the
    // host to the view first, and say so in the log. The refusal stays in the engine
    // as the backstop; this is the honest reading of the user's intent, not a reroute
    // of a message that was addressed somewhere else.
    if (viewId !== null && hostState && hostState.session_id !== viewId) {
      pageLog(`send: host is on ${hostState.session_id}, moving it to the viewed session ${viewId}`)
      try {
        await invoke('switch_session', { id: viewId })
        hostState = await invoke<HostState>('session_state')
      } catch (e) {
        pageLog(`send: could not move the host to ${viewId}: ${e}`)
      }
    }
    try {
      await invoke('start_turn', { text, attachments: atts, session: viewId ?? 0 })
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
    // The sidebar spinner is the one piece of state that outlives a missed event:
    // a turn that ended while this tab was asleep would spin forever. The host runs
    // one turn at a time, so its answer is authoritative for the whole set.
    if (!h.busy && runningSessions.size) {
      runningSessions = new Set()
    } else if (h.busy && h.session_id && !runningSessions.has(h.session_id)) {
      runningSessions = new Set(runningSessions).add(h.session_id)
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
    // Looking is not working. This used to end in `switch_session`, so every click
    // in the sidebar moved the engine: the workspace line changed, a cold-cache note
    // appeared, and a glance at another conversation rewrote the state of the one
    // you were reading. The host now follows the *composer*: a turn carries the
    // conversation it was sent from, and that is the moment the engine moves.
    await resyncView(id)
  }

  async function resyncView(id: number) {
    const mySeq = ++resyncSeq
    resyncing++
    viewId = id
    // The transcript owns buttons that name a conversation (export), so it needs
    // the id of what is on screen - which is not the same as the session the
    // engine happens to be working on.
    if (transcript) transcript.sessionId = id
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
    if (transcript) transcript.sessionId = 0
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
    pageLog(`history: ${page.messages.length} row(s) for session ${id}`)
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
    // Geometry checks need a window that is actually on screen. WebKitGTK hands an
    // occluded or blanked page a zero-size viewport, so every rect measures 0 and
    // one locked screen produced fourteen failures that read like CSS bugs.
    // Measure nothing, say so once, and stop rather than inventing twelve.
    const offScreen = () => {
      if (window.innerWidth < 300 || window.innerHeight < 300)
        return `the window measures ${window.innerWidth}x${window.innerHeight}`
      if (document.visibilityState !== 'visible') return `the document is ${document.visibilityState}`
      const t = document.querySelector('.transcript') as HTMLElement | null
      if (!t) {
        // Synthetic blocks are painted into the live transcript. If the view is a
        // draft or the empty screen, that transcript is not in the document at all,
        // every rect reads 0, and the failures read like CSS bugs.
        return 'no transcript is mounted (the view is a draft or the empty screen)'
      }
      if (t.getBoundingClientRect().width < 50) {
        // Which of the three causes it is changes the fix: an empty screen mounted
        // over the transcript, a zero-size viewport from an occluded window, or a
        // layout pass that never ran. Say what the DOM actually looks like.
        const r = t.getBoundingClientRect()
        const empty = !!document.querySelector('.empty')
        return (
          `.transcript measures ${Math.round(r.width)}x${Math.round(r.height)} ` +
          `(empty screen shown=${empty}, turns=${document.querySelectorAll('.turn').length}, ` +
          `viewport ${window.innerWidth}x${window.innerHeight})`
        )
      }
      return null
    }
    // The layout checks need a mounted transcript, not a draft view. Opening the
    // conversation the user was in is what a person would do; measuring a detached
    // tree is what a broken harness does.
    async function ensureTranscriptMounted() {
      // Measured, not merely present: the empty screen can leave the transcript in the
      // document with a zero box, which measures exactly as badly as being absent.
      if ((document.querySelector('.transcript')?.getBoundingClientRect().width ?? 0) >= 50) return true
      const reopen = async (id: number, how: string) => {
        const err = await invoke('switch_session', { id }).then(() => null, (e) => String(e))
        // The RPC moves the host; `openSession` moves the *view*, which is what these
        // checks need. Doing only the first left the app showing a draft with the host
        // pointed somewhere else, and the scroller stayed `display: none`.
        await openSession(id).catch(() => {})
        // History paints into the transcript asynchronously. A check that clears the
        // transcript and writes its own blocks must not race that paint, or the element
        // it measures is a history row that arrives afterwards - which is exactly how
        // "expanded thinking block has no width: 0" came back even with the view open.
        await waitFor('history to paint', () => document.querySelectorAll('.turn').length > 0, 4000)
        await new Promise<void>((r) => requestAnimationFrame(() => r()))
        // Width, not presence: the transcript element stays in the document while the
        // empty screen is up, and a 0x0 box measures just as uselessly as none.
        const ok = await waitFor(
          'a laid-out transcript',
          () => (document.querySelector('.transcript')?.getBoundingClientRect().width ?? 0) >= 50,
          4000,
        )
        pageLog(
          `selftest note: ${how} session ${id} for the layout checks: laid out=${ok}, ` +
            `viewId=${viewId}, msgs=${viewMsgCount}, busy=${busy}, turns=${document.querySelectorAll('.turn').length}` +
            `${err ? `, switch failed: ${err}` : ''}`,
        )
        return ok
      }
      // ---- what the footer claimed before anyone clicked it ------------------
      // The toolbar states two facts the window does not need to start a turn:
      // which search provider is live, and whether the gateway is up. Both used to
      // be fetched only when their popover opened, so the pill printed "unknown"
      // until it was interrogated - and for the gateway, an "off" that a phone
      // contradicts is a claim about a network-exposed agent.
      {
        const footVal = (label: string) => {
          const b = [...document.querySelectorAll('footer button.fbtn')].find(
            (x) => x.querySelector('.flbl')?.textContent?.trim() === label,
          )
          return (b?.querySelector('.fsum')?.textContent ?? '<no control>').trim()
        }
        const searchVal = await waitFor(
          'the footer to state the search provider',
          () => footVal('search') !== 'unknown' && footVal('search') !== '',
          4000,
        )
          ? footVal('search')
          : footVal('search')
        const view = await invoke<{ backend: string }>('search_settings')
        const allowed =
          view.backend === 'auto'
            ? ['searxng', 'no container runtime']
            : view.backend === 'searxng'
              ? ['searxng']
              : ['duckduckgo']
        if (!allowed.some((a) => searchVal.startsWith(a)))
          fail(
            `the search control says "${searchVal}" before anyone opened it, while the engine says ` +
              `backend=${view.backend} (no boot fetch, so the toolbar is a placeholder)`,
          )
        const g = await invoke<{ running: boolean; port: number; note: string }>('gateway_status')
        const gwVal = footVal('gateway')
        if (gwVal === 'unknown' || gwVal === '<no control>')
          fail(`the gateway control says "${gwVal}" while the engine reports running=${g.running}`)
        else if (g.running && !gwVal.includes(String(g.port)))
          fail(`the gateway control says "${gwVal}" while it is serving on :${g.port}`)
        else if (!g.running && !gwVal.startsWith('off'))
          fail(`the gateway control says "${gwVal}" while it is not running`)
        else
          pageLog(
            `footer truth ok (search "${searchVal}" for backend=${view.backend}, ` +
              `gateway "${gwVal}" running=${g.running})`,
          )
      }

      // A session with messages, not merely the first row. An empty session keeps the
      // empty screen up, the scroller stays `display: none`, and every rect still
      // reads 0: opening "a session" is not the premise, opening one that renders is.
      const sessions = await invoke<Session[]>('list_sessions')
      const back = sessions.find((s) => s.message_count > 0)?.id
      if (back !== undefined) return reopen(back, 'reopened')
      // A scratch store (scripts/selftest.sh runs against a throwaway data dir) starts
      // empty, so there is nothing to reopen and the run used to abort having measured
      // nothing. Make the content instead: one scripted turn, and the checks get the
      // same premise they get everywhere else.
      const st0 = await invoke<HostState>('session_state')
      await invoke('start_turn', {
        text: 'Selftest wipe check: reply with the single word ok.',
        session: 0,
      }).catch((e) => pageLog(`selftest note: could not start the bootstrap turn: ${e}`))
      for (let i = 0; i < 120; i++) {
        const st = await invoke<HostState>('session_state')
        if (st.turns > st0.turns) break
        await new Promise((r) => setTimeout(r, 250))
      }
      const made = (await invoke<Session[]>('list_sessions')).find((s) => s.message_count > 0)?.id
      if (made === undefined) {
        pageLog('selftest note: no session with content, and the bootstrap turn produced none')
        return false
      }
      return reopen(made, 'created')
    }
    // Geometry checks are only meaningful while the page is being laid out. The
    // window can leave that state halfway through a run - minimized, occluded,
    // switched workspace - and then every rect reads 0. Counting those as CSS bugs
    // produced fourteen failures about a locked screen, so the premise is checked
    // where the measuring happens and the skip is reported by name.
    let endpointSkips = 0
    // A turn that never finished is a product bug only if the endpoint could have
    // finished one. This box has been prefilling at 17 tok/s while a 50 GB model
    // serves someone else: a ~2k-token prompt then needs three minutes of prefill
    // alone, and every check that needs a completed turn fails in a chain. Attribute
    // first, then skip by name, then refuse to call the run clean.
    // scripts/mock-llm.mjs advertises itself: model id `cwoffee-mock`, build_info
    // `cwoffee-mock ...`. Under a scripted endpoint there is no prefill to blame -
    // it answers in milliseconds and plays the same scenario every run - so a turn
    // that cannot finish is ours, and behaviour that used to be a coin flip
    // ("did the model call the tool?") can be asserted instead of excused.
    const scriptedEndpoint = () =>
      models.some((m) => m.id === 'cwoffee-mock' || (m.server ?? '').startsWith('cwoffee-mock'))
    const endpointTooSlow = async (where: string) => {
      if (scriptedEndpoint()) return false
      if (lastPrefill === null || lastPrefill >= 50) return false
      endpointSkips++
      pageLog(
        `selftest note: skipped ${where}; the endpoint prefills at ${lastPrefill.toFixed(1)} tok/s, so a turn cannot complete inside the harness budget`,
      )
      return true
    }
    let blindSkips = 0
    const blind = async (where: string) => {
      let b = offScreen()
      if (!b) return false
      // One recovery attempt first. A view that drifted onto a draft is a harness
      // problem that can be fixed on the spot; a locked screen cannot be.
      await ensureTranscriptMounted()
      b = offScreen()
      if (!b) {
        pageLog(`selftest note: the view had drifted for ${where}; reopened a session`)
        return false
      }
      blindSkips++
      pageLog(`selftest note: skipped ${where}; ${b}`)
      return true
    }
    if (await blind('at the start')) {
      pageLog('SELFTEST ABORTED: nothing was measured')
      return
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
        // Reasoning is pinned too, for the duration of the run, and restored
        // afterwards. The harness tests wiring - approvals, tool cards, live blocks -
        // and an xhigh local model can spend three minutes thinking before it emits a
        // single `exec` call: one run measured 24k delta frames and no answer text
        // before the approval even arrived. A gate that costs twenty minutes of
        // inference on a single-slot endpoint is a gate nobody runs. Thinking is still
        // exercised at medium; the block, its duration label and its reveal are all
        // asserted below.
        await invoke('set_reasoning', { level: 'medium' })
        pageLog(
          `selftest pinned mode=auto reasoning=medium (was mode=${mode0}, reasoning=${reasoning0})`,
        )
      }
      const idFloor = Math.max(0, ...(await invoke<Session[]>('list_sessions')).map((s) => s.id))
      await sweepSelftest('start', idFloor)
      // Whose conversation the host was on before this suite barged in. Every turn
      // below names the session it belongs to, and the run ends by putting the host
      // back here: a harness that leaves the host parked in its own scratch session -
      // or worse, sends into the user's open one - is the "the app is mutating my
      // conversations" complaint, aimed at us.
      const session0 = (await invoke<HostState>('session_state')).session_id
      // A step that is about to send has to move the *host*, not just the view: the
      // host has one active conversation and refuses a turn addressed to another.
      // `resyncView` paints; this moves both and proves they agree before sending.
      async function useSession(id: number) {
        await invoke('switch_session', { id })
        await resyncView(id)
        const h = await invoke<HostState>('session_state')
        if (h.session_id !== id) fail(`useSession(${id}): the host is on ${h.session_id}`)
      }
      const before = await invoke<Session[]>('list_sessions')
      await invoke('new_session', { title: 'selftest' })
      await new Promise((r) => setTimeout(r, 400))
      const mid = await invoke<Session[]>('list_sessions')
      if (mid.length !== before.length + 1) fail(`new_session: ${before.length} -> ${mid.length}`)
      const foundTarget = mid.find((s) => s.title === 'selftest')
      if (!foundTarget) return fail('selftest session not listed')
      // Reassigned once: the session-CRUD test deletes this row, and the rest of the
      // suite needs a live one.
      let target: Session = foundTarget
      // Everything below sends turns, so move the host into the scratch session now
      // instead of leaving it in the user's conversation and getting every turn
      // refused - or, before the addressing rule existed, answered there.
      await useSession(target.id)

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

      // That delete removed the row the rest of the suite is built on. Anything that
      // still names it sends turns into a session id the store no longer has, and the
      // symptoms are unrelated-looking: "the workspace lock did not fire" (the engine
      // looks the row up and finds nothing), "0 messages after reload", "no attachment
      // chip". So re-open a scratch session and re-anchor before going on.
      {
        const reborn = await invoke<number>('new_session', { title: 'selftest' })
        const listed = (await invoke<Session[]>('list_sessions')).find((x) => x.id === reborn)
        if (!listed) fail(`the scratch session ${reborn} was not listed after being recreated`)
        else {
          target = listed
          await useSession(reborn)
        }
      }

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
      let stNow = await invoke<HostState>('session_state')
      // Exclude the view as well as the host session: comparing a session against
      // itself is how this step reported "DOM unchanged" while doing nothing.
      const eligible = (list: Session[], hostId: number) =>
        list.find((s) => s.id !== hostId && s.id !== viewId && (s.message_count ?? 0) > 0)
      let other = eligible(after, stNow.session_id)
      if (!other) {
        // The premise, not the assertion: a scratch store (scripts/selftest.sh runs
        // against a throwaway data dir) starts with whatever one turn the harness had
        // to create for layout, so "some other session with messages" has to be made
        // rather than hoped for.
        const madeBefore = (await invoke<HostState>('session_state')).turns
        newSession()
        await invoke('start_turn', {
          text: 'Selftest wipe check: reply with the single word ok.',
          session: 0,
        }).catch(() => {})
        for (let i = 0; i < 120; i++) {
          const stt = await invoke<HostState>('session_state')
          if (stt.turns > madeBefore) break
          await new Promise((r) => setTimeout(r, 250))
        }
        const again = await invoke<Session[]>('list_sessions')
        // Re-take the baseline: making the extra session moved the host, and the
        // invariant below is "opening a conversation does not move the host from
        // where it is now", not from where it was before the harness went fishing.
        stNow = await invoke<HostState>('session_state')
        other = eligible(again, stNow.session_id) ?? again.find((s) => (s.message_count ?? 0) > 0 && s.id !== viewId)
      }
      if (!other) fail('no stored session with messages available to open, even after making one')
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
        // Browsing is not working. Opening a conversation moves the *view* and
        // leaves the engine where it was: a click in the sidebar used to run a full
        // session switch, which re-adopted that session's workspace and re-announced
        // a cold prompt cache, so reading someone else's conversation printed a
        // workspace-change note and a cache warning that looked spontaneous. The host
        // follows a turn, not a mouse.
        const stAfter = await invoke<HostState>('session_state')
        if (stAfter.session_id !== stNow.session_id)
          fail(`opening a session moved the host: ${stNow.session_id} -> ${stAfter.session_id}`)
        const noise = [...document.querySelectorAll('.blk-note, .blk-warn')]
          .map((n) => n.textContent ?? '')
          .filter((t) => /prompt cache|workspace/i.test(t))
        if (noise.length)
          fail(`browsing announced host plumbing: ${noise[0].slice(0, 110)}`)
        // The folder chip has to follow the conversation on screen, not the one the
        // engine happens to be working in - otherwise the chip lies while you read.
        const wanted = sessions.find((x) => x.id === other.id)?.workspace ?? ''
        // Scoped to the composer: the sidebar has its own .wsrow rows, and the
        // first match in the document is a folder group, not the chip.
        const chip = document.querySelector('.cbox .wsrow span')?.textContent?.trim() ?? ''
        if (wanted && chip && !chip.includes(wanted.split('/').filter(Boolean).pop() ?? ''))
          fail(`composer folder chip shows ${chip} while viewing ${wanted}`)
        pageLog(
          `openSession(${other.id}): view moved, host stayed on ${stAfter.session_id}, ` +
            `transcript reloaded=${domChanged}, no host noise`,
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
        // Vision over the wire is the thing under test, not how long the model
        // deliberates about a white pixel: reasoning off keeps this a two-second
        // turn instead of a five-minute one.
        // The host has to be *on* the session a turn names. An earlier check opens a
        // real conversation, and a turn aimed at a different one is refused - correctly,
        // but it reads as "the attachment path is broken".
        await useSession(target.id)
        await invoke('set_reasoning', { level: 'off' })
        await invoke('start_turn', {
          text: 'What colour is this image? Answer in one word.',
          attachments: [{ mime: 'image/png', data_b64: PNG1x1, name: 'probe.png' }],
          session: target.id,
        })
        const done = await waitTurnDone(turnsBefore)
        await invoke('set_reasoning', { level: 'medium' })
        restore()
        if (!done) {
          // One stalled endpoint turn must not become ten downstream failures: say
          // which check is being skipped, release the slot, and carry on. The
          // attachment path itself is still covered by the wire check below, which
          // does not need the model to answer.
          pageLog('note: the vision turn did not finish on this endpoint; skipping the store check that follows it')
          for (const e of errs) pageLog(`SELFTEST note: endpoint said: ${e}`)
          await settleTurn('after a stalled vision turn')
        } else {
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
      }

      // A here-document write used to make the approval card taller than the
      // window: Allow/Deny sat below the fold, nothing scrolled, and the only
      // way out was to let the approval time out. The card caps its height and
      // the quoted command scrolls inside it. Synthetic event on purpose - this
      // must not wait for a model to decide to write a wall of text.
      {
        const wall =
          'cat > notes.md <<\'EOF\'\n' +
          Array.from({ length: 300 }, (_, i) => `line ${i} of prose about the harrow road branch library`).join('\n') +
          '\nEOF  [+299 more lines, 20118 characters total]'
        handle({
          kind: 'approvalRequested',
          d: { id: 990001, tool: 'exec', risk: 'sensitive', summary: wall },
        } as unknown as EngineEvent)
        await new Promise((r) => setTimeout(r, 90))
        const body = document.querySelector('.appr-body') as HTMLElement | null
        const card = document.querySelector('.approval-card') as HTMLElement | null
        const deny = Array.from(document.querySelectorAll('.approval-card button')).find((b) =>
          /deny/i.test(b.textContent ?? ''),
        ) as HTMLElement | null
        if (!body || !card || !deny) {
          fail('the long-command approval card did not render all its parts')
        } else {
          const vp = window.innerHeight
          if (card.getBoundingClientRect().bottom > vp + 1)
            fail(`the approval card runs past a ${vp}px window (${Math.round(card.getBoundingClientRect().bottom)}px)`)
          if (body.scrollHeight <= body.clientHeight + 4)
            fail(`the quoted command does not scroll (${body.scrollHeight} <= ${body.clientHeight}): the card grew instead of capping`)
          if (deny.getBoundingClientRect().bottom > vp + 1)
            fail(`the Deny button sits ${Math.round(deny.getBoundingClientRect().bottom - vp)}px below the fold`)
          // The scroll has to stay inside the card. `overscroll-behavior: contain`
          // is the whole reason: without it, wheeling through a long command
          // pushes the transcript behind the card instead.
          const sc = document.querySelector('.scroller') as HTMLElement | null
          const beforeTop = sc?.scrollTop ?? 0
          body.scrollTop = 400
          await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)))
          if (body.scrollTop < 200) fail(`the command body refused to scroll (scrollTop ${body.scrollTop})`)
          if (sc && Math.abs(sc.scrollTop - beforeTop) > 1) fail('scrolling the command moved the transcript behind it')
          pageLog(
            `approval scroll ok (body shows ${body.clientHeight}px of ${body.scrollHeight}px, deny ${Math.round(vp - deny.getBoundingClientRect().bottom)}px above the fold)`,
          )
        }
        approval = null
        await new Promise((r) => setTimeout(r, 40))
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
        // A session of its own. The session this check used to run in already holds
        // the image turn from the attachment check, and a quantized model with a
        // picture in context does strange things: three runs of this step streamed
        // 7.5k-24k frames, parsed no tool call at all, and never produced an approval
        // to answer. A step that needs a clean context should ask for one.
        const apprSess = await invoke<number>('new_session', { title: 'selftest approval' })
        await invoke('switch_session', { id: apprSess })
        await new Promise((r) => setTimeout(r, 500))
        const approvalTurnsBefore = await turnCount()
        await invoke('set_mode', { mode: 'ask' })
        // Two changes here, both about what the step can actually prove. The call is
        // a read *outside* the workspace: it is one line of arguments, it is gated in
        // ask mode, and it also proves the per-call risk class and that the approval
        // names the escape. The old prompt asked for `exec echo ...`, and this
        // checkpoint sometimes spends minutes emitting tool-call JSON that never
        // completes - chars=0, 7.5k to 24k delta frames, no approval ever arriving -
        // which failed the suite on a message about the previous turn. Thinking is off
        // for the same reason: the round trip is under test, not how much the model
        // reflects. The thinking path is asserted separately, with thinking on.
        await invoke('set_reasoning', { level: 'off' })
        await invoke('start_turn', {
          text: 'Use read_file to read the file /etc/hostname and tell me what it says.',
          session: apprSess,
        })
        const dl = performance.now() + 150000
        while (!approval && performance.now() < dl) {
          await new Promise((r) => setTimeout(r, 200))
        }
        if (!approval) {
          fail('no approval request arrived for the read-outside-the-workspace call')
          // Do not leave the turn running: every check after this one would blame it.
          await settleTurn('approval turn', 5000)
        } else {
          // Read through the named type: the harness raises and clears a synthetic
          // prompt earlier in this run, and the checker would otherwise narrow the
          // live state to `never` here and refuse to describe what it sees. The
          // sentinel keeps the assertions readable and still fails loudly.
          const asked: ApprovalPrompt =
            (approval as ApprovalPrompt | null) ?? { id: 0, tool: '<none>', risk: '<none>', summary: '<none>' }
          if (!approval) fail('the approval prompt vanished between the wait and the read')
          if (asked.tool !== 'read_file') fail(`the gated call was ${asked.tool}, expected read_file`)
          if (asked.risk !== 'sensitive')
            fail(`a read outside the workspace was classified ${asked.risk}, expected sensitive`)
          if (!/OUTSIDE the workspace: \/etc\/hostname/.test(asked.summary ?? ''))
            fail(`the approval does not name the escape: ${JSON.stringify(asked.summary)}`)
          // The card is what a person decides on. The state object can be perfectly
          // correct while the card never renders - an `{#if}` guard, a CSS rule, a
          // layer moved behind the composer - and then there is nothing to click and
          // the turn dies on a timeout. So check the pixels the decision depends on.
          const card = document.querySelector('.approval-card') as HTMLElement | null
          if (!card) {
            fail('the approval card never appeared')
          } else {
            const txt = (card.textContent ?? '').replace(/\s+/g, ' ')
            if (!txt.includes('/etc/hostname'))
              fail(`the approval card does not name the file: ${txt.slice(0, 90)}`)
            if (!/sensitive/i.test(txt)) fail(`the approval card does not show the risk: ${txt.slice(0, 90)}`)
            const head = card.firstElementChild?.textContent ?? ''
            if (/read_file/.test(head)) fail(`the approval header shows the raw tool id: ${head.trim()}`)
            if (!/reading a file/i.test(head)) fail(`the approval header is not plain English: ${head.trim()}`)
          }
          // `allowSession`, because the model usually verifies such a read with a
          // shell command, and a second parked approval would time out at 300s. This
          // is also the exact spelling a phone sends - the gateway used to reject it.
          await answer('allowSession')
          await new Promise((r) => setTimeout(r, 80))
          if (document.querySelector('.approval-card'))
            fail('the approval card is still up after the answer - the decision looks unaddressed')
          // Answer every prompt the turn raises, not only the first. `allowSession`
          // grants a *tool*, never an escape, so when the model verifies its own read
          // with a shell command a second prompt appears - and a prompt nobody answers
          // parks the turn for the whole approval timeout, which is exactly what made
          // this step time out and take the rest of the suite with it.
          let answered = 1
          const end = performance.now() + 120000
          while (!evs.length && performance.now() < end) {
            if (approval) {
              await answer('allowSession')
              answered++
            }
            await new Promise((r) => setTimeout(r, 200))
          }
          if (answered > 1) pageLog(`approval step answered ${answered} prompt(s)`)
          if (!evs.length) fail('tool never finished after allow')
          else if (!evs[0].endsWith('true')) fail(`tool failed after allow: ${evs[0]}`)
        }
        restore()
        restore()
        // Waiting on `busy` alone can pass before the host has even started the turn.
        // If the turn outlives the budget, stop it and say so rather than failing the
        // steps that come after it.
        if (!(await waitTurnDone(approvalTurnsBefore, 120000))) {
          if (!(await settleTurn('approval turn', 10000))) fail('approval turn never ended')
          else pageLog('approval turn was stopped rather than waited out')
        }
        // Only now: the session is the one the turn ran in, and deleting it while a
        // turn is live is refused (correctly) by the engine.
        await invoke('delete_session', { id: apprSess }).catch(() => {})
        // That step left the host on a session it just deleted. Pull it back to the
        // scratch conversation before anything else tries to send.
        await useSession(target.id)
      }
      // That block needed `ask`; nothing after it does. Leaving the mode there
      // means the next check that happens to make the model reach for `exec`
      // parks on an approval nobody is listening for, and the check reads as a
      // hung turn rather than a harness that changed the rules three steps back.
      await invoke('set_mode', { mode: 'auto' })
      await invoke('set_reasoning', { level: 'medium' })

      // And make sure nothing is still running: the workspace steps below read host
      // state, and the engine answers "a turn is running, so the workspace change is
      // queued behind it" - a true statement that reads as a broken refusal.
      if (!(await settleTurn('before the workspace checks'))) fail('host would not go idle')

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
      let wsNow = (await invoke<HostState>('session_state')).workspace
      if (wsNow !== wsBefore) fail(`a non-directory workspace was adopted: ${wsNow}`)
      pageLog(`workspace refusal: ${refused}`)

      // The system temp folder is where sessions live before anyone chooses a
      // folder. Accepting it would split "no workspace" across two sidebar groups,
      // and it is writable, so only an explicit rule keeps it out.
      refused = ''
      try {
        await invoke('set_workspace', { path: '/tmp' })
        fail('the system temp folder was accepted as a workspace')
      } catch (e) {
        refused = cleanErr(e)
      }
      if (!/temp folder/i.test(refused))
        fail(`the temp refusal does not explain itself: ${JSON.stringify(refused)}`)

      // A session keeps the folder it started in. Moving one makes the sidebar
      // grouping decoration - the row would still name the old folder while
      // `write_file` landed in the new one.
      //
      // Settle again immediately before asking. The two checks above each waited on
      // the host, and a turn that a previous step cancelled can leave a successor
      // starting right about here; the engine then answers "a turn is running, so the
      // workspace change is queued behind it", which is a correct answer to a question
      // the check is not asking.
      if (!(await settleTurn('before the move check', 8000))) fail('host would not go idle')
      // The lock is a property of *a session*, so the host has to be in one. A draft
      // (session 0) has no folder of its own, and moving a draft's workspace is legal
      // - checking the lock there would be checking nothing.
      await useSession(target.id)
      {
        const h = await invoke<HostState>('session_state')
        const row = (await invoke<Session[]>('list_sessions')).find((s) => s.id === h.session_id)
        // Printed because "the lock did not fire" has three causes and they need
        // different fixes: the host was in a draft, the session had no stored folder,
        // or the engine really did move it.
        pageLog(
          `move check: host session=${h.session_id} state.ws=${h.workspace} stored=${JSON.stringify(row?.workspace ?? null)} refusal_seq=${h.refusal_seq}`,
        )
        if (!h.session_id) fail(`the move check ran with the host in draft (session ${h.session_id})`)
        if (!row?.workspace) fail(`the move check ran on a session with no stored folder`)
      }
      const otherDir = '/home/dandandelion/Projects'
      refused = ''
      try {
        await invoke('set_workspace', { path: otherDir })
        fail(`a session in ${wsBefore} was moved to ${otherDir}`)
      } catch (e) {
        refused = cleanErr(e)
      }
      if (!/keeps its folder/i.test(refused))
        fail(`the move refusal does not explain itself: ${JSON.stringify(refused)}`)
      wsNow = (await invoke<HostState>('session_state')).workspace
      if (wsNow !== wsBefore) fail(`a refused move still landed: ${wsNow}`)

      // The other half of the rule, and the reason locking the move is not a
      // dead end: a session created *in* a folder starts there.
      {
        const made = await invoke<number>('new_session', {
          title: 'selftest ws folder',
          workspace: otherDir,
        })
        const listed = (await invoke<Session[]>('list_sessions')).find((s) => s.id === made)
        if (!listed) fail('a session created in a folder is not listed')
        else if (listed.workspace !== otherDir)
          fail(`a session created in ${otherDir} says its folder is ${listed.workspace}`)
        await invoke('delete_session', { id: made })
      }
      pageLog(
        `workspace: rejected a bad path, rejected /tmp, rejected moving ${wsBefore}; a new session takes the folder it is given`,
      )

      // --- browsing during a live turn -------------------------------------
      // This is the reported bug: clicking another session while the model was
      // answering appeared to do nothing, and the running turn's thinking block
      // was painted over whatever session was on screen, which read as a
      // conversation that had lost its history.
      const sessA = await invoke<number>('new_session', { title: 'selftest live A' })
      await useSession(sessA)
      const sessB = await invoke<number>('new_session', { title: 'selftest live B' })
      // `new_session` moves the host to the row it just made, and the view follows.
      // The turn has to belong to A while B is only what the user is *looking at*, so
      // re-anchor on A here; otherwise send() writes into B and every assertion below
      // is about a conversation that was never meant to be running.
      await useSession(sessA)
      const turnsBeforeLive = (await invoke<HostState>('session_state')).turns
      // Go through send(), not invoke(): the user message and the turn's owner
      // are recorded there, and replaying a partial turn depends on them.
      draft = 'Count out loud from one to fifteen, one number per line.'
      await send()

      // Generous on purpose: a cancelled predecessor can keep the endpoint's single
      // slot busy for tens of seconds, and a 30s budget turned "the model is slow to
      // answer" into "session switching during a turn is broken".
      const sawStream = await waitFor('stream to start', () => busy && turnSessionId === sessA, 90_000)
      if (!sawStream) fail('turn never started for the live-browsing check')

      if (sawStream) {
        // The row of the session that is answering says *answering*: a turning arc,
        // not the unread pulse. Those are two different questions - "is it working?"
        // and "did something arrive while I was away?" - and one dot was answering
        // both, so a running session looked unread and an unread one looked busy.
        const rowA = [...document.querySelectorAll('.sessrow')].find((r) =>
          (r.querySelector('.sess-title')?.textContent ?? '').includes('selftest live A'),
        )
        if (!rowA) fail('the running session has no row in the sidebar')
        else if (!rowA.querySelector('.sess-dot.busy'))
          fail('the session that is inferencing shows no spinner in the sidebar')

        // Switch the view while the turn is still running - and *through the click
        // path*, because that is where the complaint lived: the host queues a session
        // switch behind the running turn, and awaiting that call froze the click until
        // the turn ended. "Changing session mid-conversation is not allowed", measured.
        const t0 = performance.now()
        await openSession(sessB)
        const switchMs = performance.now() - t0
        if (viewId !== sessB) fail(`view did not move during a turn: viewId=${viewId}`)
        if (switchMs > 3000) fail(`switching sessions during a turn took ${Math.round(switchMs)}ms`)
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
        // This check is about browsing during a turn, not about how long a local
        // model takes to think about fifteen numbers: xhigh on a loaded endpoint has
        // run past three minutes here. Waiting it out makes the harness fail on a busy
        // box, and worse, every step after it fails with "cannot delete the active
        // session while a turn is running" and the report blames the delete check.
        // So: wait a bounded time, then stop the turn the way a user would and prove
        // the host is idle before anything else touches it.
        if (!(await waitTurnDone(turnsBeforeLive, 120000))) {
          await invoke('cancel_turn')
          const idleBy = performance.now() + 30000
          while (performance.now() < idleBy) {
            if (!(await invoke<HostState>('session_state')).busy) break
            await new Promise((r) => setTimeout(r, 250))
          }
          if ((await invoke<HostState>('session_state')).busy) fail('cancel_turn left the host busy')
          pageLog('live-browsing turn was cancelled at 60s rather than waited out')
        }
        // A spinner that outlives its turn is worse than no spinner.
        await refreshState()
        if (document.querySelector('.sess-dot.busy'))
          fail('a finished turn left a running marker in the sidebar')
      }
      await invoke('delete_session', { id: sessA })
      await invoke('delete_session', { id: sessB })
      await useSession(target.id)

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
        const stalled =
          (await waitTurnDone(turnsBeforeDraft)) === false &&
          (await endpointTooSlow('the draft-session checks'))
        if (!stalled && !(await waitTurnDone(turnsBeforeDraft, 1000)))
          fail('draft first turn never finished')
        if (!stalled) {
          const afterSend = await invoke<Session[]>('list_sessions')
          if (afterSend.length !== beforeDraft.length + 1) {
            fail(`first message did not create exactly one row: ${beforeDraft.length} -> ${afterSend.length}`)
          }
          const mine = afterSend.find((x) => x.title.includes('6 times 7'))
          if (!mine)
            fail(`the new row was not titled by its first message: ${afterSend.slice(0, 3).map((x) => x.title).join(' | ')}`)
          if (!(await waitFor('view to leave the empty state', () => !showEmpty, 5000))) {
            fail(`view stayed empty after sending (viewId=${viewId} msgCount=${viewMsgCount} busy=${busy})`)
          }
          if (mine) await invoke('delete_session', { id: mine.id })
        }
      }

      // --- the search panel reports the engine, and never changes the choice ----
      {
        const btn = [...document.querySelectorAll('footer button.fbtn')].find(
          (b) => b.querySelector('.flbl')?.textContent?.trim() === 'search',
        ) as HTMLButtonElement | undefined
        if (!btn) {
          fail('the footer has no search control, so a phone cannot see which search is on')
        } else {
          btn.click()
          // The panel reads its state on first open, including a container probe
          // (podman ps), so give the round trip room.
          if (!(await waitFor('the search panel to fill in', () => document.querySelectorAll('.pop .pop-item').length > 3, 4000))) {
            fail('the search panel opened but never listed its providers')
          } else {
            const items = [...document.querySelectorAll('.pop .pop-item')] as HTMLElement[]
            const labels = items.map((i) => (i.querySelector('.pop-lbl')?.textContent ?? '').trim())
            const want = ['DuckDuckGo', 'SearXNG, run by Cwoffee', 'SearXNG, already running']
            const missing = want.filter((w) => !labels.includes(w))
            if (missing.length)
              fail(`the search panel is missing provider(s): ${missing.join(' | ')} (saw ${labels.join(' | ')})`)
            const marked = items.filter((i) => i.getAttribute('aria-checked') === 'true')
            if (marked.length !== 1)
              fail(`exactly one provider must be marked as in use, saw ${marked.length}: ${labels.join(' | ')}`)
            const inUse = items.find((i) => i.textContent?.includes('In use now'))
            const said = (inUse?.querySelector('.pop-sub')?.textContent ?? '').trim()
            if (!inUse || said.length < 6)
              fail(
                `the panel does not say what search is actually doing (got ${said || 'nothing'}): a card that prints the config value cannot tell "auto" with a container from "auto" with no runtime`,
              )
            // And the mark must agree with the engine, not with a stale local copy.
            const view = await invoke<{ backend: string; describing: string }>('search_settings')
            const expect =
              view.backend === 'auto'
                ? 'run by Cwoffee'
                : view.backend === 'searxng'
                  ? 'already running'
                  : 'DuckDuckGo'
            const text = marked[0]?.textContent ?? ''
            if (!text.includes(expect))
              fail(`the panel marks "${text.trim()}" while the engine says backend=${view.backend}`)
            // Deliberately no click on a provider: choosing one writes the user's
            // config file. A self-test that changes which search they use is the kind
            // of side effect the harness promised to stop causing.

            // Geometry of the panel. The user found by eye what a rect check finds
            // for free: the managed-container block prints three lines, and every
            // `.pop-sub` is pinned to grid row 2, so line 3 was painted on top of
            // line 2 into one garbled sentence. Measure it, and measure it on a
            // synthetic status block so the check runs whatever provider is set.
            const panel = document.querySelector('.pop') as HTMLElement | null
            if (!panel) fail('the search panel popover element vanished mid-check')
            else {
              const pr = panel.getBoundingClientRect()
              const footer = document.querySelector('footer')?.getBoundingClientRect()
              if (pr.width < 200 || pr.height < 60)
                fail(`the search panel has no size: ${pr.width.toFixed(0)}x${pr.height.toFixed(0)}`)
              if (pr.top < -1 || pr.bottom > innerHeight + 1)
                fail(`the search panel is off-screen vertically: ${pr.top.toFixed(0)}..${pr.bottom.toFixed(0)} of ${innerHeight}`)
              if (pr.left < -1 || pr.right > innerWidth + 1)
                fail(`the search panel is cut off sideways: ${pr.left.toFixed(0)}..${pr.right.toFixed(0)} of ${innerWidth}`)
              // A panel that opens upward from the footer should end at the footer's
              // top; a pixel of overlap is rounding, not a layout fault. What must not
              // happen is the panel sitting *inside* or behind the footer.
              if (footer && pr.bottom > footer.top + 4 && pr.top < footer.bottom - 4)
                fail(
                  `the search panel sits on top of the footer it opens from: panel ${pr.top.toFixed(0)}..${pr.bottom.toFixed(0)}, footer ${footer.top.toFixed(0)}..${footer.bottom.toFixed(0)}`,
                )

            const overlaps = (a: Element, b: Element) => {
              const x = a.getBoundingClientRect(),
                y = b.getBoundingClientRect()
              return (
                Math.min(x.right, y.right) - Math.max(x.left, y.left) > 4 &&
                Math.min(x.bottom, y.bottom) - Math.max(x.top, y.top) > 4
              )
            }
            const rows = [...panel.querySelectorAll('.pop-item')] as HTMLElement[]
            for (let i = 0; i + 1 < rows.length; i++)
              if (overlaps(rows[i], rows[i + 1]))
                fail(`provider rows overlap: "${rows[i].textContent?.trim().slice(0, 20)}" / "${rows[i + 1].textContent?.trim().slice(0, 20)}"`)

            const synth = document.createElement('div')
            synth.className = 'pop-item static status'
            const l1 = document.createElement('span')
            l1.className = 'pop-lbl'
            l1.textContent = 'Its own container'
            const l2 = document.createElement('span')
            l2.className = 'pop-sub'
            l2.textContent = 'podman · 8899 · idle in 120s · standing up'
            const l3 = document.createElement('span')
            l3.className = 'pop-sub mono s2'
            l3.textContent = 'port 8899 · idle 120s · config /home/user/.config/cwoffee/searxng'
            synth.append(l1, l2, l3)
            panel.appendChild(synth)
            await new Promise<void>((r) => requestAnimationFrame(() => setTimeout(r, 60)))
            const lineBad: string[] = []
            for (const [a, b, na, nb] of [
              [l1, l2, 'label', 'runtime line'],
              [l2, l3, 'runtime line', 'port/config line'],
              [l1, l3, 'label', 'port/config line'],
            ] as [HTMLElement, HTMLElement, string, string][]) {
              if (overlaps(a, b)) {
                const ra = a.getBoundingClientRect(),
                  rb = b.getBoundingClientRect()
                lineBad.push(
                  `${na} at y ${ra.top.toFixed(0)}..${ra.bottom.toFixed(0)} and ${nb} at ${rb.top.toFixed(0)}..${rb.bottom.toFixed(0)}`,
                )
              }
              if (a.getBoundingClientRect().width < 20 || b.getBoundingClientRect().width < 20)
                lineBad.push(`${na} or ${nb} has no width`)
            }
            synth.remove()
            if (lineBad.length)
              fail(
                `the managed-container status block stacks wrong: ${lineBad.join('; ')} ` +
                  `(every .pop-sub sits on grid row 2 unless the item says otherwise)`,
              )
            const real = panel.querySelector('.pop-item.status')
            pageLog(
              `search panel geometry ok (${rows.length} rows, ${pr.width.toFixed(0)}x${pr.height.toFixed(0)} inside ` +
                `${innerWidth}x${innerHeight}, status block stacks, live status ${real ? 'shown' : 'not applicable on this backend'})`,
            )
            }
            btn.click()
            await new Promise((r) => setTimeout(r, 150))
            pageLog(`search panel ok (${view.backend}: ${view.describing})`)
          }
        }
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
          // The premise of this check is tokens. An endpoint still in prefill after
          // six seconds has not failed it - it has not answered it. Failing here
          // blamed the UI for the model thinking. (Seen live: 131s to first token.)
          const st = await invoke<HostState>('session_state')
          if (st.busy)
            pageLog(
              `selftest note: no live block within 6s - turn ${st.turns} is running and the endpoint has emitted nothing yet, so this check skipped rather than blamed the UI`,
            )
          else fail(`expected exactly one live block during a turn, saw ${liveCount()}`)
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

          // Scope to the answer that just streamed. A document-wide query passes
          // this check on a table left over from an earlier turn, which is how a
          // render bug in the newest answer can be hidden by an older one.
          const lastAns = [...document.querySelectorAll('.blk-answer .blk-body')].pop()
          const table = lastAns?.querySelector('table') ?? null
          if (!table) {
            // Say which side failed. This check has fired with the table sitting
            // correctly in the store and absent from the DOM, and those are two
            // different bugs: a render path that never parsed the block, versus a
            // turn whose answer was never a table. Without the store text and the
            // block list in the log, every later reading of this line is a guess.
            const blocks = [...document.querySelectorAll('.transcript .blk')].map(
              (b) => (b.className.match(/blk-(\w+)/) ?? ['?', '?'])[1],
            )
            const bodies = [...document.querySelectorAll('.blk-answer .blk-body')] as HTMLElement[]
            const lastBody = bodies[bodies.length - 1]
            let stored = '(no session)'
            if (viewId !== null) {
              const page = await invoke<{ messages: { role: string; content: string }[] }>(
                'session_messages',
                { id: viewId },
              )
              const lastAssistant = [...page.messages].reverse().find((m) => m.role === 'assistant')
              stored = lastAssistant ? lastAssistant.content.slice(0, 120) : '(none)'
            }
            fail(
              `markdown table was not rendered as a table :: blocks=[${blocks.join(',')}] :: ` +
                `last answer body='${(lastBody?.innerHTML ?? '').slice(0, 120)}' :: stored='${stored.replace(/\n/g, ' ')}'`,
            )
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
            if (!/^Thought for /i.test(label))
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
          // Whether the file exists at all is the model's decision, not the app's,
          // and a quantized local model sometimes answers "the path is ..." without
          // calling the tool. Ask the store what the turn actually did, so a run
          // where nothing was written says that instead of reporting a product bug
          // - a gate that cries wolf is a gate people stop reading.
          const st = await invoke<HostState>('session_state')
          const page = await invoke<{ messages: any[] }>('session_messages', {
            id: st.session_id,
          }).catch(() => ({ messages: [] as any[] }))
          const wrote = page.messages.some(
            (m) => (m.files?.length ?? 0) > 0 || /write_file/.test(m.tool_calls_json ?? ''),
          )
          if (!wrote) {
            pageLog(
              'selftest note: the model answered without calling write_file, so there was no ' +
                'file to attach; the chip check needs that premise and skipped',
            )
          } else if (!chips.length) {
            fail('the turn wrote a file and no attachment was rendered')
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

        // --- several questions in one call: one card, one submit --------------
        {
          const sent: { id: number; text: string; options: number[] }[] = []
          transcript?.showQuestionSet(
            [
              {
                id: 900_101,
                question: 'Which format?',
                options: [{ label: 'Markdown' }, { label: 'PDF', detail: 'prints well' }],
                recommended: 1,
                allow_custom: false,
                multi: false,
                set: { id: 7001, index: 0, total: 2 },
              },
              {
                id: 900_102,
                question: 'Anything else the document should say?',
                options: [{ label: 'Keep it short' }],
                allow_custom: true,
                multi: false,
                set: { id: 7001, index: 1, total: 2 },
              },
            ],
            (id, r) => sent.push({ id, ...r }),
          )
          const setCard = document.querySelector('.blk-ask.is-set')
          if (!setCard) {
            fail('a set of questions did not render as one card')
          } else {
            const count = () => setCard.querySelector('.ask-count')?.textContent?.trim() ?? ''
            if (count() !== 'Question 1 of 2') fail(`the set card does not say which question it is on: ${count()}`)
            if (setCard.querySelectorAll('.ask-submit').length !== 1)
              fail('a set must offer exactly one submit, not one per question')
            const submitBtn = () => setCard.querySelector<HTMLButtonElement>('.ask-submit')!
            if (!submitBtn().disabled) fail('a set with nothing answered must not offer Send')
            if (setCard.querySelectorAll('.ask-opts').length !== 1)
              fail('both questions are on screen at once; a set is answered one at a time')

            // answer the first
            click(setCard.querySelectorAll('.ask-opt')[1])
            if (count() !== 'Question 1 of 2') fail('answering jumped to another question by itself')
            if (submitBtn().disabled) fail('one answer is enough to send the set (the rest go as no answer)')
            if (!/1 of 2/.test(submitBtn().textContent ?? ''))
              fail(`the send button must say how much is answered: ${submitBtn()?.textContent}`)
            const hint = setCard.querySelector('.ask-hint')?.textContent ?? ''
            if (!/would be sent as no answer/.test(hint))
              fail(`the card does not warn that a question is still unanswered: ${hint || 'nothing'}`)

            // navigate, and check the first answer survives the trip
            click(setCard.querySelector('.ask-next'))
            if (count() !== 'Question 2 of 2') fail(`Next did not advance: ${count()}`)
            const q2 = setCard.querySelector('.ask-q')?.textContent ?? ''
            if (!q2.startsWith('Anything else')) fail(`the second question is not shown: ${q2}`)
            const inp = setCard.querySelector('.ask-input') as HTMLInputElement | null
            if (!inp) fail('the second question offers no typed answer')
            else {
              inp.value = 'add the cost per unit'
              inp.dispatchEvent(new Event('input', { bubbles: true }))
            }
            click(setCard.querySelector('.ask-back'))
            if (count() !== 'Question 1 of 2') fail(`Back did not return: ${count()}`)
            if (!setCard.querySelector('.ask-opt.is-sel'))
              fail('navigating back lost the answer given to the first question')
            click(setCard.querySelector('.ask-next'))

            if (submitBtn().disabled) fail('both answered but Send is still disabled')
            if (submitBtn().textContent?.trim() !== 'Send answers')
              fail(`a fully answered set should offer a plain Send: ${submitBtn()?.textContent}`)
            click(submitBtn())

            if (sent.length !== 2) fail(`a set must answer every question exactly once; got ${sent.length}`)
            const a = sent.find((s) => s.id === 900_101)
            const b = sent.find((s) => s.id === 900_102)
            if (!a || a.options[0] !== 1 || a.text !== 'PDF') fail(`first answer came out wrong: ${JSON.stringify(a)}`)
            if (!b || b.text !== 'add the cost per unit') fail(`second answer came out wrong: ${JSON.stringify(b)}`)
            if (!setCard.classList.contains('answered')) fail('the set card did not collapse on submit')
            // Settled: nothing left to answer. The complaint about the old card was
            // that it stayed interactive forever after the call had returned.
            if (setCard.querySelectorAll('.ask-opt, .ask-input, .ask-submit, .ask-back, .ask-next').length)
              fail('a submitted set still holds interactive controls')
            const label = setCard.querySelector('.blk-label')?.textContent?.trim() ?? ''
            if (label !== 'answered 2 of 2') fail(`the settled head line reads "${label}"`)
            const qa = setCard.querySelectorAll('.ask-qa-item')
            if (qa.length !== 2) fail(`the expanded summary should hold both pairs, got ${qa.length}`)
            else {
              const txt = [...qa].map((n) => n.textContent ?? '').join(' | ')
              if (!txt.includes('Which format?') || !txt.includes('Anything else'))
                fail(`the summary lost the questions: ${txt}`)
              if (!txt.includes('PDF') || !txt.includes('add the cost per unit'))
                fail(`the summary lost the answers: ${txt}`)
            }
            const qaEl = setCard.querySelector('.ask-qa') as HTMLElement
            if (getComputedStyle(qaEl).display !== 'none')
              fail('a submitted set opens with its whole Q&A in the way')
            click(setCard.querySelector('.blk-head'))
            if (getComputedStyle(qaEl).display === 'none')
              fail('the settled set does not expand to show what was asked and answered')
            setCard.remove()
          }

          // Declining a set is an explicit no to all of it, and the card still closes.
          const noes: { id: number; text: string }[] = []
          transcript?.showQuestionSet(
            [
              { id: 900_111, question: 'a?', options: [{ label: 'x' }], allow_custom: false, multi: false, set: { id: 7002, index: 0, total: 2 } },
              { id: 900_112, question: 'b?', options: [{ label: 'y' }], allow_custom: false, multi: false, set: { id: 7002, index: 1, total: 2 } },
            ],
            (id, r) => noes.push({ id, text: r.text }),
          )
          const decl = document.querySelector('.blk-ask.is-set')
          if (!decl) fail('a declined set did not render')
          else {
            click(decl.querySelector('.ask-decline'))
            if (noes.length !== 2 || noes.some((n) => n.text !== ''))
              fail(`Decline must answer every question with nothing: ${JSON.stringify(noes)}`)
            const lb = decl.querySelector('.blk-label')?.textContent?.trim() ?? ''
            if (lb !== 'declined') fail(`a declined set reads "${lb}"`)
            decl.remove()
          }
          pageLog('question set ok (one card, navigation kept answers, one submit, settled read-only)')
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
        // Empty mode is driven by the number of loaded rows, and a synthetic load
        // bypasses the loader that would have set it. Without this the whole tree is
        // `display: none` and every rect the layout checks read comes back 0.
        viewMsgCount = history.length
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

      // --- the mode tab states a direction without pretending to work -----------
      {
        const tabs = [...document.querySelectorAll('.modesel')] as HTMLButtonElement[]
        if (tabs.length !== 2) fail(`the mode tab has ${tabs.length} segments, want 2`)
        else {
          const modeBefore = (await invoke<HostState>('session_state')).mode
          tabs[1].click()
          await new Promise((r) => setTimeout(r, 300))
          const after = await invoke<HostState>('session_state')
          if (after.mode !== modeBefore)
            fail(`the Chat tab changed the host mode to ${after.mode} - it is not wired yet, and must not`)
          tabs[0].click()
        }
        // The big "+ New session" pill is gone; the action itself is not, and it has
        // to stay reachable from the sidebar as well as from Ctrl+N.
        if (!document.querySelector('.newbtn')) fail('the sidebar lost its new-session control')
      }

      // --- the file the model wrote is a chip, live and after a reload ----------
      // The report was "it wrote the file and nothing was attached". In the store the
      // engine had it right - the assistant row carries the file - which is what the
      // chip landing in *another session's* transcript looks like from the outside.
      // Two code paths are asserted here because the seam between them is where this
      // class of bug lives: the live strip built from events, and the history strip
      // rebuilt from the store when the session is reopened.
      {
        const essayPath = 'notes/selftest-espresso.txt'
        await useSession(target.id)
        const turnsBefore = await turnCount()
        await invoke('set_reasoning', { level: 'off' })
        await invoke('start_turn', {
          text: `Use write_file to save about 50 words about espresso to ${essayPath}. Do not run any other tool.`,
          session: target.id,
        })
        if (!(await waitTurnDone(turnsBefore))) fail('the essay turn never ended')
        if (!document.querySelector('.filechip')) {
          // Name the state rather than the symptom: whether the row arrived at all
          // is what separates "the engine lost it" from "the UI did not draw it".
          const rows = await invoke<any>('session_messages', { id: target.id, limit: 4 })
          pageLog(
            `no live chip; last rows: ${JSON.stringify(
              (rows.messages ?? []).map((m: any) => ({ id: m.id, role: m.role, files: m.files?.length ?? 0 })),
            )}`,
          )
        }
        const live = [...document.querySelectorAll('.filechip')] as HTMLElement[]
        const want = live.find((c) => (c.dataset.path ?? '').includes('selftest-espresso'))
        if (!live.length) fail('a file-producing turn rendered no attachment chip at all')
        else if (!want)
          fail(`no chip for ${essayPath}; saw ${live.map((c) => c.dataset.path).join(', ')}`)
        else if (!want.querySelector('svg.file-glyph'))
          // An emoji glyph from the system font renders in full colour, which is the
          // "coloured icons break the theming" report. It must be the inline icon.
          fail('the file chip is using a text glyph, not an inline monochrome icon')

        // And the same file after a trip through the store. Two facts, checked
        // separately: the engine stamped the file on a stored row, and the transcript
        // painted a chip for it. Only the second one depends on the view being still
        // here - an event-driven resync of another session can steal it mid-check.
        const rows = await invoke<any>('session_messages', { id: target.id, limit: 8 })
        const stamped = (rows.messages ?? []).some((m: any) =>
          (m.files ?? []).some((f: any) => String(f.path ?? '').includes('selftest-espresso')),
        )
        if (!stamped)
          fail(
            `the engine did not attach the output file to any stored row of session ${target.id} (${(rows.messages ?? []).length} rows read)`,
          )
        if (viewId !== target.id) await resyncView(target.id)
        const again = [...document.querySelectorAll('.filechip')] as HTMLElement[]
        if (!again.some((c) => (c.dataset.path ?? '').includes('selftest-espresso')))
          fail(`the attachment is in the store but not on screen (viewId=${viewId})`)

        // Leave no litter. The UI has no file API, so the cheapest removal is the
        // model's own tool, in auto mode with thinking off: one short turn.
        const cleanBefore = await turnCount()
        await invoke('start_turn', {
          text: `Delete the file ${essayPath} using exec with: rm -f ${essayPath}. Reply with the single word done.`,
          session: target.id,
        })
        if (!(await waitTurnDone(cleanBefore))) pageLog('selftest: the cleanup turn did not finish')
        await invoke('set_reasoning', { level: 'medium' })
      }

      // --- what a scripted endpoint makes testable -----------------------------
      // Everything above that needs a specific model behaviour feeds the transcript
      // by hand, because against a live model the behaviour is a coin flip: the
      // model may not call the tool, may call another one, may answer in prose.
      // Under scripts/mock-llm.mjs the scenario decides what the model does, so the
      // seams themselves get driven end to end - engine event -> App buffering ->
      // transcript card, streamed markdown -> committed blocks, broken arguments ->
      // repair -> run, dead stream -> a visible error - in seconds, with no prefill
      // and nobody's prompt cache on the line.
      if (scriptedEndpoint()) {
        await invoke('set_mode', { mode: 'auto' })
        await useSession(target.id)

        // (a2) bullets one at a time, not four at once
        {
          const before = await turnCount()
          let stopped = false
          const liCounts: number[] = []
          const ulCounts: number[] = []
          let markerPainted = ''
          let firstItemTop: number | null = null
          let firstItemText = ''
          const sampler = (async () => {
            while (!stopped) {
              const all = [...document.querySelectorAll('.blk-answer .blk-body')]
              const body = all[all.length - 1]
              if (body) {
                const lis = [...body.querySelectorAll('li')]
                liCounts.push(lis.length)
                ulCounts.push(body.querySelectorAll('ul').length)
                const text = body.textContent ?? ''
                // The marker characters belong to the syntax, not to the sentence.
                if (!markerPainted && /(^|\n)\s*[-*+]\s/.test(text)) markerPainted = text.slice(0, 60)
                if (firstItemTop === null && lis.length >= 2) {
                  // Relative to the list, not to the viewport: the view is being
                  // scrolled by the stream itself, so a viewport coordinate moves
                  // 165px for reasons that have nothing to do with the bullet.
                  const ul = lis[0].parentElement?.getBoundingClientRect()
                  firstItemTop = lis[0].getBoundingClientRect().top - (ul?.top ?? 0)
                  firstItemText = lis[0].textContent ?? ''
                }
              }
              await new Promise((r) => setTimeout(r, 40))
            }
          })()
          await invoke('start_turn', {
            text: 'Selftest bullets-live: four short bullets about coffee, then one sentence.',
            session: target.id,
          })
          const done = await waitTurnDone(before, 60000)
          stopped = true
          await sampler
          const all = [...document.querySelectorAll('.blk-answer .blk-body')]
          const body = all[all.length - 1]
          const lis = body ? [...body.querySelectorAll('li')] : []
          // The last sample taken while the turn was still running is what proves
          // the structure arrived during the stream rather than at turnEnd.
          const running = liCounts.slice(0, Math.max(0, liCounts.length - 1))
          const maxRunning = running.length ? Math.max(...running) : 0
          const steps = new Set(running).size
          if (!done) fail('the scripted bullet turn never finished')
          else if (!body) fail('the scripted bullet turn left no answer block')
          else if (lis.length !== 4) fail(`the bullet list ended with ${lis.length} items, expected 4`)
          else if (maxRunning < 3)
            fail(
              `only ${maxRunning} of 4 bullets were real <li> elements while the stream was still running ` +
                `(counts ${running.slice(0, 12).join(',')}): the list waited for its blank line and then appeared all at once`,
            )
          else if (steps < 2)
            fail(`the item count never grew during the stream (saw ${[...new Set(running)].join(', ')})`)
          else if (markerPainted)
            fail(`a list marker was painted as text: "${markerPainted}"`)
          else if (Math.max(...ulCounts) > 1)
            fail(
              `the stream built ${Math.max(...ulCounts)} separate lists out of one: ` +
                'items must join the list that already exists',
            )
          else if (body.querySelectorAll('a').length !== 1)
            fail(`the link inside a bullet did not become a link (${body.querySelectorAll('a').length} anchors)`)
          else {
            const ulNow = lis[0].parentElement?.getBoundingClientRect()
            const nowTop = lis[0].getBoundingClientRect().top - (ulNow?.top ?? 0)
            if (firstItemTop !== null && Math.abs(nowTop - firstItemTop) > 1)
              fail(`the first bullet moved ${Math.abs(nowTop - firstItemTop).toFixed(1)}px while the list finished - settled text was rewritten`)
            if (firstItemText && (lis[0].textContent ?? '') !== firstItemText)
              fail(`the first bullet's text changed while the list finished: "${firstItemText}" -> "${lis[0].textContent}"`)
            pageLog(
              `scripted bullets ok (${maxRunning} of 4 were structure while streaming, ` +
                `counts ${[...new Set(running)].join('->')}, one <ul>, markers never painted, ` +
                `first bullet moved ${(Math.abs(nowTop - (firstItemTop ?? nowTop))).toFixed(1)}px)`,
            )
          }
        }

        // (a3) an inline construct that straddles two paints
        // The real model streams roughly a token at a time and the UI paints about
        // every 120ms, so a `**bold**` span almost always arrives across two or three
        // paints. The mock's chunking used to be coarser than the paint interval,
        // which is why every inline check passed while the user saw literal asterisks.
        {
          const before = await turnCount()
          let stopped = false
          let starSeen = ''
          let strongWhileStreaming = 0
          const sampler = (async () => {
            while (!stopped) {
              const all = [...document.querySelectorAll('.blk-answer .blk-body')]
              const body = all[all.length - 1]
              if (body) {
                const t = body.textContent ?? ''
                if (!starSeen && t.includes('*')) starSeen = t.slice(0, 70)
                if (body.querySelectorAll('strong').length > 0) strongWhileStreaming += 1
              }
              await new Promise((r) => setTimeout(r, 40))
            }
          })()
          await invoke('start_turn', {
            text: 'Selftest bold-split-live: three bullets, two bold words, one word after.',
            session: target.id,
          })
          const done = await waitTurnDone(before, 60000)
          stopped = true
          await sampler
          const all = [...document.querySelectorAll('.blk-answer .blk-body')]
          const body = all[all.length - 1]
          const text = body ? body.textContent ?? '' : ''
          if (!done) fail('the scripted bold turn never finished')
          else if (!body) fail('the scripted bold turn left no answer block')
          else if (starSeen)
            fail(
              `a literal asterisk reached the screen while a bold span was being written: "${starSeen}" ` +
                `- the inline stream and the DOM host disagreed about where the next word goes`,
            )
          else if (body.querySelectorAll('strong').length !== 2)
            fail(`expected 2 bold spans, found ${body.querySelectorAll('strong').length}`)
          else if (!text.includes('Sourcing sets the ceiling'))
            fail(`characters were dropped from a split construct: "${text.slice(0, 120)}"`)
          else if (strongWhileStreaming < 3)
            fail(
              `bold appeared in only ${strongWhileStreaming} samples during the stream, i.e. only at the end - ` +
                'a construct should become bold as it closes, not when the block commits',
            )
          else
            pageLog(
              `scripted bold ok (2 spans, no literal asterisks, bold present in ${strongWhileStreaming} mid-stream samples, ` +
                `nothing dropped from a construct split across paints)`,
            )
        }

        // (z) a line that starts like a table and is not one still arrives
        {
          // Same session, same wait primitive as every other turn in this file.
          // A store-counter poll returns before the page has finished the turn, and
          // the next check then streams into a transcript whose previous block had
          // not closed yet - which measures the harness, not the renderer.
          const before = await turnCount()
          await invoke('start_turn', {
            text: 'Selftest fake-table-head: one line starting with a pipe, then one prose line.',
            session: target.id,
          }).catch((e) => pageLog(`selftest note: fake-table-head turn failed: ${e}`))
          if (!(await waitTurnDone(before)))
            fail('the fake-table-head turn never finished')
          const bodies = [...document.querySelectorAll('.blk-answer .blk-body')]
          const body = bodies[bodies.length - 1]
          const said = (body?.textContent ?? '').replace(/\s+/g, ' ')
          if (!said.includes('has to reach the screen unchanged'))
            fail(
              `a pipe-led line with no delimiter row lost its text: body='${said.slice(0, 120)}' - ` +
                `tableHeadPending waits for the delimiter row, and waiting is only allowed to delay, never to drop`,
            )
          else if (body?.querySelector('table'))
            fail('a line with no delimiter row was rendered as a table')
          else pageLog(`fake table head ok (held back while ambiguous, then painted: "${said.slice(0, 44)}...")`)
        }

        // (a5) the model opens a spreadsheet the user already has
        //
        // Until `sheet_read` the engine could make an .xlsx and could not look at
        // one: read_file calls it binary. The interesting failure of a new tool is
        // not a crash, it is the model calling it and getting nothing useful back,
        // so this check reads the *card*, which holds what the engine really
        // returned, and cross-checks the paging hint against the rows shown. The
        // scenario carries `require` strings too: a tool result that never reached
        // the prompt fails the turn instead of being papered over by a scripted
        // answer that sounds right.
        {
          const before = await turnCount()
          const ws = (hostState?.workspace ?? '').replace(/\/+$/, '')
          const file = `${ws}/selftest-regions.csv`
          if (!ws) fail('the self-test host has no workspace, so the sheet-read fixture path is unknown')
          await invoke('start_turn', {
            text:
              `Selftest sheet-read: put a four-row region table into ${file} (Region, Units, Price) ` +
              'and then read it back with the spreadsheet tool and tell me the first two rows.',
            session: target.id,
          }).catch((e) => pageLog(`selftest note: sheet-read turn failed: ${e}`))
          const done = await waitTurnDone(before, 60000)
          const cards = [...document.querySelectorAll('.blk-tool[data-tool="sheet_read"]')]
          const card = cards[cards.length - 1]
          const out = card?.querySelector('.tool-out')?.textContent ?? ''
          const rows = [...out.matchAll(/^(\d+):/gm)].map((m) => Number(m[1]))
          const hint = out.match(/ask again with range [A-Z]+(\d+):/)
          if (!done) fail('the scripted sheet-read turn never finished')
          else if (!card)
            fail('no tool card for sheet_read: the model never reached the spreadsheet tool')
          else if (!out.includes('Region') || !out.includes('North'))
            fail(`the sheet_read card shows no cells - output was "${out.slice(0, 180)}"`)
          else if (!out.includes('ask again with range'))
            fail(`a 6-cell cap returned the whole table without a continuation: "${out.slice(0, 220)}"`)
          else if (!rows.length) fail(`the grid has no row prefixes: "${out.slice(0, 180)}"`)
          else if (hint && Number(hint[1]) !== Math.max(...rows) + 1)
            fail(
              `the continuation names row ${hint[1]} but the last row shown was ${Math.max(...rows)}: ` +
                'a range the model copies verbatim would silently skip rows',
            )
          else {
            // The answer is the last thing to arrive, so its presence is what says
            // the tool loop ran to the end rather than stopping after one call.
            const answers = [...document.querySelectorAll('.blk-answer .blk-body')].map(
              (b) => b.textContent ?? '',
            )
            const said = answers.some((t) => t.includes('North 12'))
            if (!said)
              fail(
                `the grid arrived but the answer did not: ${answers.length} answer block(s), ` +
                    'the scripted loop stopped before its last step - check the tool results in the prompt',
              )
            else
              pageLog(
                `sheet_read ok (grid rows ${rows[0]}-${rows[rows.length - 1]}, cap held at 6 cells, ` +
                  `continuation starts at row ${hint![1]}, the grid the card shows is what the prompt got)`,
              )
          }
        }

        // (a) structure while the tokens arrive, not when they stop
        {
          const before = await turnCount()
          let stopped = false
          let headingAt: number | null = null
          let tableAt: number | null = null
          let listAt: number | null = null
          const rowCounts: number[] = []
          // When structure arrives late, the useful question is what the body held
          // while the tokens arrived. Keep a short ring of that, so a failure names
          // the shape on screen instead of only the symptom.
          const trace: string[] & { lastTick?: number } = [] as string[] & { lastTick?: number }
          const tTrace = performance.now()
          const sampler = (async () => {
            while (!stopped) {
              const all = [...document.querySelectorAll('.blk-answer .blk-body')]
              const body = all[all.length - 1]
              if (body) {
                const now = performance.now()
                if (now - (trace.lastTick ?? 0) > 150) {
                  trace.lastTick = now
                  trace.push(
                    `${Math.round(now - tTrace)}ms [${[...body.children].map((c) => c.tagName).join(' ')}] ` +
                      (body.innerHTML ?? '').replace(/<[^>]+>/g, ' ').replace(/\s+/g, ' ').slice(0, 44),
                  )
                  if (trace.length > 40) trace.shift()
                }
                if (headingAt === null && body.querySelector('h1, h2, h3')) headingAt = now
                if (tableAt === null && body.querySelector('table')) tableAt = now
                if (listAt === null && body.querySelector('li')) listAt = now
                rowCounts.push(body.querySelectorAll('tbody tr').length)
              }
              await new Promise((r) => setTimeout(r, 40))
            }
          })()
          await invoke('start_turn', {
            text:
              'Show me a markdown table of three brewing methods with columns Method, Grind and Time, then a bullet list.',
            session: target.id,
          })
          const done = await waitTurnDone(before, 60000)
          const endedAt = performance.now()
          stopped = true
          await sampler
          const all = [...document.querySelectorAll('.blk-answer .blk-body')]
          const body = all[all.length - 1]
          const rows = body ? body.querySelectorAll('table tr').length : 0
          const items = body ? body.querySelectorAll('li').length : 0
          // How long before the end of the turn the structure was already on screen.
          const ago = (t: number | null) => (t === null ? -1 : Math.round(endedAt - t))
          if (!done) fail('the scripted table turn never finished')
          else if (!body) fail('the scripted table turn left no answer block')
          else if (headingAt === null)
            fail(`the heading never became a heading :: ${trace.slice(-8).join(' :: ')}`)
          else if (tableAt === null) fail('the table never became a table; the answer stayed text')
          else if (ago(tableAt) < 60)
            fail(`the table only appeared ${ago(tableAt)}ms before turnEnd - that is at turnEnd, not during`)
          else if (rows !== 4) fail(`the table has ${rows} rows (header counted), expected 4`)
          else if (
            (() => {
              const running = rowCounts.slice(0, Math.max(0, rowCounts.length - 1))
              return running.length ? Math.max(...running) : 0
            })() < 2
          )
            fail(
              `only ${Math.max(...rowCounts.slice(0, -1))} of 3 table rows existed while the stream was still ` +
                `running (counts ${rowCounts.slice(0, 14).join(',')}) - the table waited for its block to end`,
            )
          else if (items !== 3)
            fail(
              `the bullet list has ${items} items, expected 3; ` +
                `answer bodies=${all.length}, last body children=[${body ? [...body.children].map((c) => c.tagName.toLowerCase() + (c.className ? '.' + c.className : '')).join(' ') : 'none'}], ` +
                `raw tail=${JSON.stringify((body?.textContent ?? '').slice(-70))}`,
            )
          else
            pageLog(
              `scripted structure ok (heading ${ago(headingAt)}ms and table ${ago(tableAt)}ms before turnEnd, ` +
                `rows arrived ${[...new Set(rowCounts)].join('->')} during the stream, ` +
                `${rows - 1} data rows, ${items} list items)`,
            )
        }

        // (b) one ask_user call, two questions, one card
        {
          const before = await turnCount()
          await invoke('start_turn', {
            text: 'Ask me two questions before writing the report, then write it.',
            session: target.id,
          })
          let askCard: HTMLElement | null = null
          const cardSeen = await waitFor(
            'the scripted two-question card',
            () => {
              const cards = [...document.querySelectorAll('.blk-ask.is-set')] as HTMLElement[]
              const c = cards[cards.length - 1]
              askCard = c && c.querySelectorAll('.ask-opt').length ? c : null
              return askCard !== null
            },
            60000,
          )
          const card = askCard as HTMLElement | null
          if (!cardSeen || !card) {
            fail('the scripted ask_user set produced no card: the App-side buffering never saw both questions')
          } else {
            const count = card.querySelector('.ask-count')?.textContent?.trim() ?? ''
            if (!/1\s*of\s*2/.test(count)) fail(`a two-question card does not say which one it is on: "${count}"`)
            if (card.querySelectorAll('.ask-submit').length !== 1)
              fail(`a two-question set renders ${card.querySelectorAll('.ask-submit').length} submit buttons, expected one`)
            const pick = (i: number) => {
              const opts = [...card.querySelectorAll('.ask-opt')] as HTMLElement[]
              if (!opts.length) fail('the question pane has no options to pick')
              else opts[Math.min(i, opts.length - 1)].click()
            }
            const next = () => (card.querySelector('.ask-next') as HTMLElement | null)?.click()
            pick(0)
            await new Promise((r) => setTimeout(r, 90))
            if (!card.querySelector('.ask-next')) fail('a two-question card has no Next control')
            next()
            await new Promise((r) => setTimeout(r, 120))
            pick(1)
            await new Promise((r) => setTimeout(r, 90))
            const back = card.querySelector('.ask-back') as HTMLElement | null
            if (!back) fail('a two-question card has no Back control')
            else {
              back.click()
              await new Promise((r) => setTimeout(r, 120))
              const kept = card.querySelectorAll('.ask-opt.is-sel, .ask-opt[aria-checked="true"]').length
              if (kept !== 1) fail(`navigating back lost the first answer (${kept} still selected)`)
              next()
              await new Promise((r) => setTimeout(r, 120))
            }
            ;(card.querySelector('.ask-submit') as HTMLElement)?.click()
            await new Promise((r) => setTimeout(r, 250))
            const done = await waitTurnDone(before, 60000)
            if (!card.classList.contains('answered')) fail('submitting a set left the card interactive')
            else if (card.querySelector('.ask-opt') || card.querySelector('.ask-submit'))
              fail('a settled set still carries controls the user could press again')
            const pairs = [...card.querySelectorAll('.ask-qa-item')]
            const receipt = card.querySelector('.ask-answered')?.textContent?.trim() ?? ''
            const answers = pairs.map((r) => r.querySelector('.ask-qa-a')?.textContent?.trim() ?? '')
            const questions = pairs.map((r) => r.querySelector('.ask-qa-q')?.textContent?.trim() ?? '')
            if (!done) fail('the turn after the scripted questions never finished')
            else if (pairs.length !== 2) fail(`the settled set shows ${pairs.length} question/answer pairs, expected 2`)
            else if (!answers.every((a) => a.length > 0)) fail(`a settled answer is blank: ${JSON.stringify(answers)}`)
            else if (!/answered 2 of 2/.test(receipt)) fail(`the settled card reads "${receipt}"`)
            else
              pageLog(
                `scripted ask set ok (one tool call, two questions, one card; settled "${receipt}": ` +
                  `${questions.map((q, i) => `${q} -> ${answers[i]}`).join(' | ').slice(0, 160)})`,
              )
          }
        }

        // (c) arguments a quantised model mangled: repaired, run, and admitted
        {
          const errs: string[] = []
          const restore = beginEventTap((e) => {
            if (e.kind === 'error') errs.push(e.d.message)
          })
          const before = await turnCount()
          await invoke('start_turn', {
            text: 'Call a tool with arguments that are missing a brace.',
            session: target.id,
          })
          const done = await waitTurnDone(before, 60000)
          restore()
          const said = errs.filter((m) => /repaired malformed arguments/i.test(m))
          const tools = [...document.querySelectorAll('.blk-tool')]
          if (!done) fail('the broken-arguments turn never finished')
          else if (!said.length)
            fail(`a braceless tool call ran without the engine saying so; events: ${JSON.stringify(errs).slice(0, 200)}`)
          else if (!tools.length) fail('the repaired call left no tool card in the transcript')
          else pageLog(`scripted repair ok (${said[0].slice(0, 96)})`)
        }

        // (d) a stream that dies mid-answer must be reported, not truncated quietly
        {
          const errs: string[] = []
          const restore = beginEventTap((e) => {
            if (e.kind === 'error') errs.push(e.d.message)
          })
          const before = await turnCount()
          await invoke('start_turn', {
            text: 'Answer and then stop halfway through the tokens.',
            session: target.id,
          })
          const done = await waitTurnDone(before, 40000)
          restore()
          const died = errs.filter((m) => /transport|connection|stream|closed|interrupt|eof/i.test(m))
          if (!died.length)
            fail(`an endpoint that dropped mid-stream surfaced nothing to the user; events: ${JSON.stringify(errs).slice(0, 200)}`)
          else pageLog(`scripted dead stream ok (${died[0].slice(0, 96)}; turn ended=${done})`)
        }

        await sweepSelftest('after the scripted turns')
      }

      // --- a finished card is one line of English, and nothing else ------------
      // The complaint was that the transcript read as a dump of API calls: a raw
      // tool id as the header, and a clamped block of JSON under every one of them.
      // It should read as a list of things that happened, each one openable.
      {
        const hist: StoredMessage[] = [
          {
            id: 90_020,
            session_id: 0,
            role: 'user',
            content:
              'Please make a short note file and then correct one line in it, and keep this message long enough that the bubble has a measurable width of its own, which is what this assertion is about.',
            tool_call_id: null,
            tool_calls_json: '',
            reasoning: '',
            reasoning_signature: '',
            reasoning_ms: 0,
            files: [],
            created_at: 0,
            attachments: [],
            changes: [],
          },
          {
            id: 90_021,
            session_id: 0,
            role: 'assistant',
            content: 'Two things done.',
            reasoning: 'First the file, then the line inside it.',
            reasoning_signature: '',
            reasoning_ms: 4200,
            tool_call_id: null,
            tool_calls_json: JSON.stringify([
              { id: 'h1', name: 'write_file', arguments: '{"path":"note.md","content":"a\\n"}' },
              { id: 'h2', name: 'edit_file', arguments: '{"path":"note.md","old_text":"a","new_text":"b"}' },
            ]),
            files: [{ path: 'note.md', name: 'note.md', mime: 'text/markdown', bytes: 12 }],
            created_at: 0,
            attachments: [],
            changes: [],
          },
          {
            id: 90_022,
            session_id: 0,
            role: 'tool',
            content: 'wrote 1 line',
            tool_call_id: 'h1',
            tool_calls_json: '',
            reasoning: '',
            reasoning_signature: '',
            reasoning_ms: 0,
            files: [],
            created_at: 0,
            attachments: [],
            changes: [{ path: 'note.md', added: 1, removed: 0, created: true }],
          },
          {
            id: 90_023,
            session_id: 0,
            role: 'tool',
            content: 'replaced 1 occurrence',
            tool_call_id: 'h2',
            tool_calls_json: '',
            reasoning: '',
            reasoning_signature: '',
            reasoning_ms: 0,
            files: [],
            created_at: 0,
            attachments: [],
            changes: [{ path: 'note.md', added: 1, removed: 1, created: false }],
          },
        ]
        // Measuring a hidden tree proves nothing: `.empty-mode` hides the scroller,
        // so every rect below would read 0 and the assertions would be theatre. The
        // view has to be a session with messages before synthetic history is painted.
        transcript?.clear()
        transcript?.loadHistory(hist)
        viewMsgCount = hist.length
        // Only now is there anything to measure: empty mode is driven by the number of
        // loaded rows, and measuring a hidden tree proves nothing because every rect
        // in it reads 0.
        {
          const shown =
            viewId === target.id &&
            getComputedStyle(document.querySelector('.scroller')!).display !== 'none'
          if (!shown)
            fail(
              `the transcript is hidden or belongs elsewhere while the layout checks ran (viewId=${viewId}, want ${target.id})`,
            )
        }

        const RAW =
          /^(write_file|edit_file|read_file|exec|web_search|web_fetch|download_file|write_xlsx|ask_user)$/
        const cards = [...document.querySelectorAll('.blk-tool')] as HTMLElement[]
        if (cards.length !== 2) fail(`expected 2 tool cards, saw ${cards.length}`)
        for (const c of cards) {
          const nm = (c.querySelector('.tool-name')?.textContent ?? '').trim()
          if (RAW.test(nm)) fail(`a tool header still names the tool id: "${nm}"`)
          if (!/^[A-Z]/.test(nm)) fail(`a tool header is not a sentence: "${nm}"`)
          const det = c.querySelector('.tool-detail') as HTMLElement
          if (getComputedStyle(det).display !== 'none')
            fail(`a collapsed card still shows a body: ${nm}`)
        }
        // ...and one click opens it.
        ;(cards[0].querySelector('.blk-head') as HTMLElement).click()
        await new Promise((r) => setTimeout(r, 200))
        if (getComputedStyle(cards[0].querySelector('.tool-detail') as Element).display === 'none')
          fail('clicking a card header did not open it')
        ;(cards[0].querySelector('.blk-head') as HTMLElement).click()
        await new Promise((r) => setTimeout(r, 200))
        if (getComputedStyle(cards[0].querySelector('.tool-detail') as Element).display !== 'none')
          fail('clicking the header again did not collapse it')

        // Settled thinking is a header line: "Thought for 4s" and no sneak peek.
        const rbody = document.querySelector('.blk-reasoning .blk-body') as HTMLElement
        if (!rbody) fail('no thinking block in the stored transcript')
        else {
          if (!rbody.classList.contains('settled')) fail('stored thinking is not marked settled')
          if (getComputedStyle(rbody).display !== 'none')
            fail('settled thinking shows a body nobody asked for')
          const rlabel = document.querySelector('.blk-reasoning .blk-head')?.textContent ?? ''
          if (!/^Thought for /i.test(rlabel))
            fail(`thinking header is not a duration: ${JSON.stringify(rlabel)}`)
        }

        // Outdented label, aligned content, right-side bubble for your own turn.
        const col = (document.querySelector('.transcript') ?? document.body) as HTMLElement
        const ab = document.querySelector('.blk-answer > .blk-body') as HTMLElement
        const ub = document.querySelector('.blk-user > .blk-body') as HTMLElement
        if (!ab || !ub) fail('missing answer or user body to measure')
        else {
          const abr = ab.getBoundingClientRect()
          const ubr = ub.getBoundingClientRect()
          const cr = col.getBoundingClientRect()
          if (Math.abs(abr.left - (cr.left + parseFloat(getComputedStyle(col).paddingLeft || '0'))) > 4)
            fail(`the answer no longer starts on the column edge: ${Math.round(abr.left)} vs ${Math.round(cr.left)}`)
          if (Math.abs(ubr.right - abr.right) > 3)
            fail(`the user bubble does not end where the answer ends: ${Math.round(ubr.right)} vs ${Math.round(abr.right)}`)
          const share = ubr.width / Math.max(1, ab.getBoundingClientRect().width)
          if (ubr.width > col.clientWidth * 0.78)
            fail(`the user bubble is ${(share * 100).toFixed(0)}% of the column, want <= 78%`)
          if (getComputedStyle(ub).textAlign === 'right')
            fail('text inside the user bubble is right-aligned')
          const lab = document.querySelector('.blk-answer .blk-head') as HTMLElement
          // The gutter only exists in a wide layout; in a narrow window the label
          // legitimately goes back above its block, so assert the layout that is
          // actually in force rather than the one the design prefers.
          const gutter = parseFloat(getComputedStyle(document.querySelector('.scroller')!).paddingLeft || '0')
          if (gutter >= 40 && lab.getBoundingClientRect().left > abr.left - 20)
            fail(
              `the CWOFFEE label is not outdented, so the content is still indented (label ${Math.round(lab.getBoundingClientRect().left)}, body ${Math.round(abr.left)}, gutter ${gutter})`,
            )
        }

        // Every control says it is one.
        for (const sel of ['.fbtn', '.sessrow', '.wsbtn', '.ws-plus', '.cbtn', '.btn-go']) {
          const el = document.querySelector(sel) as HTMLElement | null
          if (el && getComputedStyle(el).cursor !== 'pointer' && getComputedStyle(el).cursor !== 'not-allowed')
            fail(`${sel} does not advertise a pointer: ${getComputedStyle(el).cursor}`)
        }
        // A stored attachment belongs *under the answer that produced it*. It used to
        // be appended to the live turn, which on a reload put the chip above every
        // message in the session - off screen, so a session reopened after writing a
        // file looked like it had no attachment at all.
        const chip = document.querySelector('.filechip') as HTMLElement | null
        const answerWrap = document.querySelector('.blk-answer') as HTMLElement | null
        if (!chip) fail('a stored attachment did not render after reload')
        else if (!answerWrap) fail('no answer block to anchor the attachment to')
        else if (!(answerWrap.compareDocumentPosition(chip) & Node.DOCUMENT_POSITION_FOLLOWING))
          fail('the stored attachment is not below the answer it belongs to')
        else if (!chip.querySelector('svg.file-glyph'))
          fail('the stored attachment chip has no inline icon')

        transcript?.clear()
        pageLog('collapsed cards, english headers and bubbles ok')
      }

      // --- the streaming reveal: append-only, word by word, never rewritten ----
      // The earlier designs re-rendered the growing paragraph and folded finished
      // chunks back into it. Every version of that made already-arrived text blink
      // again about half a second after it landed, because folding is a rewrite and
      // a rewrite re-rasters. What is provable from the DOM is the property the fix
      // rests on: a node that has been painted is never replaced, and the spans
      // concatenate back to exactly what the model sent.
      {
        transcript?.clear()
        transcript?.apply({ kind: 'turnStart', d: { turn_id: 1, model: 'selftest' } })
        const deltas = [
          'thinking about the first clause, ',
          'then the second clause carries on ',
          'and a third arrives so the line has to wrap ',
          'across the block rather than down it.',
        ]
        // Remember nodes by identity. If the paint machinery ever rewrites settled
        // text again, these references stop being connected and this fails.
        const seen: HTMLElement[] = []
        const seenText: string[] = []
        let maxInFlight = 0
        let judged = 0
        // The host element must not be restyled paint by paint. Writing the reveal
        // duration as a custom property on the paragraph used to re-resolve the
        // style of every settled word inside it, every frame.
        let restyled = 0
        const styleHits: string[] = []
        const bornAt = new Map<HTMLElement, number>()
        // One word's worst-case animation window: duration cap plus the stagger
        // cap plus slack for frame jitter. Anything older than this must be fully
        // drawn, and anything younger may legitimately still be fading.
        const REVEAL_BUDGET = 200 + 100 + 40
        const nowMs = () => performance.now()
        const moRef: { o: MutationObserver | null } = { o: null }
        let watched: Element | null = null
        const watchHost = () => {
          // The body element only exists once the block has been created, and it
          // must be the same element for the whole check: attaching per paint
          // would miss the writes it is supposed to catch.
          const h = document.querySelector('.blk-reasoning .blk-body')
          if (!h || h === watched) return
          watched = h
          const mo = new MutationObserver((recs) => {
            // WebKitGTK delivers class records even with attributeFilter: ['style'],
            // so the filter is applied here as well.
            for (const r of recs)
              if (r.attributeName === 'style')
                styleHits.push(`${r.attributeName}: ${String(r.oldValue).slice(0, 40)}`)
            restyled = styleHits.length
          })
          mo.observe(h, { attributes: true, attributeFilter: ['style', 'class'] })
          moRef.o = mo
        }
        for (const d of deltas) {
          transcript?.apply({ kind: 'reasoning', d })
          // One paint per delta: the reveal unit is whatever landed since the last
          // paint, and a check that applies them in one microtask tests one chunk.
          // Spaced out so a later paint has words from an earlier one that have
          // finished: that is exactly the population the at-rest checks need.
          await new Promise((r) => requestAnimationFrame(() => setTimeout(r, 150)))
          transcript?.flushNow()
          const body = document.querySelector('.blk-reasoning .blk-body') as HTMLElement
          const tins = [...(body?.querySelectorAll('.tin') ?? [])] as HTMLElement[]
          if (!tins.length) fail('a reasoning delta produced no reveal spans')
          const wordsThisPaint = tins.length - seen.length
          watchHost()
          for (const t of seen) {
            if (!t.isConnected) fail('a revealed word was removed from the DOM after arriving')
          }
          for (const t of tins) {
            if (!seen.includes(t)) {
              seen.push(t)
              seenText.push(t.textContent ?? '')
              bornAt.set(t, nowMs())
            }
          }
          const disp = new Set(tins.map((t) => getComputedStyle(t).display))
          if (disp.has('block')) fail(`reveal spans are block-level: ${[...disp].join(',')}`)
          // Words from an earlier arrival are at rest: full opacity, and no
          // filter, because a filter takes a word out of the text layer and puts
          // it back when the animation ends - a pop per word, which at 40 words
          // a second is the blink that got the blur blamed.
          // Words that landed at least one full animation window ago. New spans
          // are appended at the end, so "at rest" is a question about age, not
          // about position in the list.
          const atRest = seen.filter((t) => (bornAt.get(t) ?? nowMs()) + REVEAL_BUDGET < nowMs())
          judged += atRest.length
          for (const t of atRest.slice(-8)) {
            const cs = getComputedStyle(t)
            if (cs.filter !== 'none') fail(`a settled word is still under a filter: ${cs.filter}`)
            if (Number(cs.opacity) < 0.98)
              fail(`a word from an earlier arrival sits at opacity ${cs.opacity} while new words land`)
          }
          // The other direction: a reveal nobody can see is not a feature. Counting
          // words mid-fade right after a paint is how "subtle but noticeable" gets
          // settled with a number instead of an opinion.
          const inFlight = tins.filter((t) => Number(getComputedStyle(t).opacity) < 0.9).length
          if (inFlight > maxInFlight) maxInFlight = inFlight
          if (inFlight > 24)
            fail(`${inFlight} words fading at once: the newest line is grey for lines of reading`)
        }
        const body = document.querySelector('.blk-reasoning .blk-body') as HTMLElement
        const br = body.getBoundingClientRect()
        const text = [...body.querySelectorAll('.tin')]
          .map((t) => t.textContent ?? '')
          .join('')
        if (text !== deltas.join(''))
          fail(`the reveal lost characters: got ${JSON.stringify(text.slice(0, 40))}...`)
        // The block must fill the column rather than stack one clause per line.
        const first = seen[0].getBoundingClientRect()
        const last = seen[seen.length - 1].getBoundingClientRect()
        if (first.top === last.top && br.width > 400)
          fail('the thinking block is one line wide, not filling the column')
        if (seen.length < deltas.length)
          fail(`${seen.length} spans for ${deltas.length} arrivals: words are being merged or dropped`)
        if (maxInFlight < 4)
          fail(`only ${maxInFlight} words were ever mid-fade at once: the reveal is invisible`)
        if (judged < 4)
          fail(`only ${judged} spans were ever old enough to judge: the reveal check sampled too early`)
        moRef.o?.disconnect()
        if (!watched) fail('no streaming paragraph to watch for style churn')
        if (restyled)
          fail(`${restyled} style writes on the streaming paragraph: ${styleHits.join(' | ')}`)
        // Newlines are kept in the part of the answer that is still raw text, or
        // an answer arrives as one grey wall until the block break appears. The
        // body itself must not keep pre-wrap: it holds real markdown elements
        // too, and the parser's own newlines would show up as blank lines.
        transcript?.apply({ kind: 'text', d: 'x\n\ny' })
        await new Promise((r) => requestAnimationFrame(() => setTimeout(r, 40)))
        transcript?.flushNow()
        const ans = document.querySelector('.blk-answer .blk-body') as HTMLElement
        const ansTail = ans.querySelector('.live-para') as HTMLElement | null
        if (!ansTail) fail('a streamed answer block is not its own paragraph element')
        transcript?.clear()
        pageLog(
          `append-only reveal ok (${seen.length} word spans, none rewritten, ` +
            `${maxInFlight} mid-fade at the busiest moment)`,
        )

      // [1] Formatting arrives while the model is still talking.
      //
      // The video evidence was a horror story streamed for fifteen seconds with
      // `**The Quiet Frequency**` on screen the whole time: the answer only
      // became markdown when the turn ended. A paragraph is now committed the
      // moment its blank line lands, and only the last, unfinished block stays
      // as raw text.
      if (await blind('before the streaming markdown checks')) return
      {
        transcript?.clear()
        transcript?.apply({ kind: 'turnStart', d: { turn_id: 2, model: 'selftest' } })
        const parts = [
          '# Roast levels\n\n',
          'Light roasts keep **bright acidity** and hold ',
          'on to whatever the origin gave them.\n\n',
          '- light: cinnamon\n- medium: city\n',
          '\n',
          'Dark roasts are still arriving and *not* committed yet',
        ]
        for (const d of parts) {
          transcript?.apply({ kind: 'text', d })
          await new Promise((r) => requestAnimationFrame(() => setTimeout(r, 40)))
          transcript?.flushNow()
        }
        const body = document.querySelector('.blk-answer .blk-body') as HTMLElement
        if (!body) fail('no answer body for the streaming markdown check')
        const h1 = body.querySelector('h1')
        if (!h1 || !/Roast levels/.test(h1.textContent ?? ''))
          fail('a heading stayed literal markdown while the answer streamed')
        if (!body.querySelector('strong')) fail('bold did not format mid-stream')
        if (!body.querySelector('ul li')) fail('a finished list did not become a list mid-stream')
        // A table has to become a table *while the answer streams*. The real-turn
        // version of this check depends on a quantised model following an
        // instruction, so it can fail for reasons that are not the render path;
        // this one feeds the exact block through the live pipeline and has no
        // such excuse. It also pins the shape the CSS needs: the table must be a
        // direct child of the body, because the styles are written `.blk-body table`.
        {
          transcript?.apply({
            kind: 'text',
            d: '\n\n| name | value |\n|---|---|\n| alpha | 1 |\n| beta | 2 |\n\n',
          })
          await new Promise((r) => requestAnimationFrame(() => setTimeout(r, 40)))
          transcript?.flushNow()
          const tb = document.querySelector('.blk-answer .blk-body') as HTMLElement
          const tbl = tb?.querySelector('table')
          if (!tbl) {
            fail(`a completed markdown table stayed literal while streaming :: '${(tb?.innerHTML ?? '').slice(0, 160)}'`)
          } else {
            if (tbl.parentElement !== tb)
              fail(`the table is nested in a ${(tbl.parentElement as HTMLElement).className} instead of sitting in the block body`)
            if (!tbl.querySelector('thead th')) fail('streamed table has no header cells')
            if (tbl.querySelectorAll('tbody tr').length < 2)
              fail(`streamed table has ${tbl.querySelectorAll('tbody tr').length} body rows`)
            if (tbl.textContent?.includes('|')) fail('stray pipes left in the streamed table')
          }
        }
        // A heading is one line long, so its own newline finishes it. Holding it
        // for a blank line left literal hash marks on screen while the model typed
        // the sentence underneath, which is the "only formats at certain points"
        // report in its most visible form.
        {
          transcript?.apply({
            kind: 'text',
            d: '\n\n## The Copy\nThe photocopier in the basement started printing again at 3:14.',
          })
          await new Promise((r) => requestAnimationFrame(() => setTimeout(r, 40)))
          transcript?.flushNow()
          const b3 = document.querySelector('.blk-answer .blk-body') as HTMLElement
          const h2 = b3?.querySelector('h2')
          if (!h2 || (h2.textContent ?? '').trim() !== 'The Copy')
            fail(`a heading waited for a blank line to format: ${(b3?.innerHTML ?? '').slice(0, 120)}`)
          const tail3 = b3?.querySelector('.live-para')
          if (tail3 && /(^|\s)#/.test(tail3.textContent ?? '')) fail('hash marks are visible while the answer streams')
          if (!tail3 || !/photocopier/.test(tail3.textContent ?? ''))
            fail('the sentence after a streamed heading is not its own live block')
        }
        const committed = [...body.querySelectorAll('h1, p, li')]
          .map((n) => n.textContent ?? '')
          .join(' ')
        if (/\*\*|(^|\s)#\s/.test(committed))
          fail(`committed markdown still shows its source: ${committed.slice(0, 60)}`)
        // A committed *plain* paragraph keeps its nodes and its box. It needs no
        // parser - the spans already sit in a `<p>` with the parser's own margins
        // - so the commit must not replace one element or move one pixel. Both are
        // measured here, because a commit that rewrites settled text is the
        // flicker this design exists to avoid, and it is invisible in a screenshot.
        {
          transcript?.apply({
            kind: 'text',
            // Its own block: the text before it had not ended, and a block that
            // contains `*` anywhere has to go through the parser.
            d: '\n\nPlain sentences with no markup whatsoever keep their spans and their box across a commit.\n\n',
          })
          await new Promise((r) => requestAnimationFrame(() => setTimeout(r, 40)))
          transcript?.flushNow()
          const body2 = document.querySelector('.blk-answer .blk-body') as HTMLElement
          const para = [...body2.querySelectorAll('p')].find((n) =>
            /no markup whatsoever/.test(n.textContent ?? ''),
          )
          if (!para) {
            fail('a plain committed paragraph is not a <p>')
          } else {
            const spans = [...para.querySelectorAll('.tin')]
            if (!spans.length)
              fail(
                `a committed plain paragraph lost its word spans :: cls='${para.className}' :: ` +
                  `bodyTins=${body2.querySelectorAll('.tin').length} :: reducedMotion=${matchMedia('(prefers-reduced-motion: reduce)').matches} :: ` +
                  `liveParas=${body2.querySelectorAll('.live-para').length} :: ${para.innerHTML.slice(0, 120)}`,
              )
            const own = para.getBoundingClientRect().top - body2.getBoundingClientRect().top
            transcript?.apply({
              kind: 'text',
              d: 'The next paragraph lands underneath and pushes nothing around.\n\n',
            })
            await new Promise((r) => requestAnimationFrame(() => setTimeout(r, 40)))
            transcript?.flushNow()
            const body3 = document.querySelector('.blk-answer .blk-body') as HTMLElement
            const gone = spans.filter((n) => !n.isConnected)
            if (gone.length)
              fail(`${gone.length} word spans were thrown away when a paragraph committed - the commit re-rasters settled text`)
            const para3 = [...body3.querySelectorAll('p')].find((n) =>
              /no markup whatsoever/.test(n.textContent ?? ''),
            )
            if (!para3) {
              fail('the committed paragraph disappeared')
            } else {
              const after = para3.getBoundingClientRect().top - body3.getBoundingClientRect().top
              if (Math.abs(after - own) > 0.6)
                fail(`committing a paragraph moved the one above it by ${(after - own).toFixed(1)}px`)
              pageLog(`commit is layout-neutral ok (${spans.length} spans kept, drift ${(after - own).toFixed(1)}px)`)
            }
          }
        }
        // The unfinished block is still raw, still revealing, and still one run.
        transcript?.apply({ kind: 'text', d: 'Dark roasts are still arriving and *not* committed yet' })
        await new Promise((r) => requestAnimationFrame(() => setTimeout(r, 40)))
        transcript?.flushNow()
        const live = document.querySelector('.blk-answer .blk-body') as HTMLElement
        const tail = live.querySelector('.live-para') as HTMLElement | null
        if (tail) {
          if (!/still arriving/.test(tail.textContent ?? ''))
            fail('the unfinished block was committed before it ended')
          if (!tail.querySelector('.tin')) fail('the tail is not revealing word by word')
        } else {
          fail('the unfinished block has no streaming paragraph')
        }
        // Ghost tail check: the reveal belongs to the newest words only. A span
        // that is still half-drawn after the animation window (200ms duration +
        // 100ms stagger cap) is text the reader has already moved past, sitting
        // grey two lines up - the flicker that got the reveal blamed.
        await new Promise((r) => setTimeout(r, 400))
        const faint = [...document.querySelectorAll('.blk-answer .tin')].filter(
          (t) => Number(getComputedStyle(t).opacity) < 0.9,
        ).length
        if (faint) fail(`${faint} word spans are still half-drawn after 400ms: the ghost tail is back`)
        pageLog('markdown mid-stream ok (h1+bold+list committed, no tail left half-drawn)')
        // Turn end leaves nothing half-rendered.
        transcript?.apply({ kind: 'turnEnd', d: { turn_id: 2, finish: 'stop' } })
        await new Promise((r) => setTimeout(r, 60))
        const done = document.querySelector('.blk-answer .blk-body') as HTMLElement
        if (done.querySelector('.live-para')) fail('a streaming paragraph survived the end of the turn')
        if (/\*\*/.test(done.textContent ?? '')) fail('markdown markers survived the end of the turn')
        if (!done.querySelector('em')) fail('the final block never got its markdown')
        transcript?.clear()
      }

      // [2b] The label in the gutter lines up with the first line of the answer
      // *while it is being generated*. It used to be propped by a streaming-only
      // `padding-top: 3px` on the body, so the name sat off the line for the whole
      // turn and snapped into place at the end. Measured on an answer that starts
      // with a paragraph, because that is what an answer usually starts with.
      {
        transcript?.clear()
        transcript?.apply({ kind: 'turnStart', d: { turn_id: 3, model: 'selftest' } })
        transcript?.apply({ kind: 'text', d: 'The first words of the answer are landing ' })
        await new Promise((r) => requestAnimationFrame(() => setTimeout(r, 40)))
        transcript?.flushNow()
        const alignPx = (where: string) => {
          const b = document.querySelector('.blk-answer .blk-body') as HTMLElement
          const who = document.querySelector('.blk-answer .blk-head') as HTMLElement
          const first = b?.firstElementChild
          const walk = document.createTreeWalker(first ?? new DocumentFragment(), NodeFilter.SHOW_TEXT)
          const tn = (first ? walk.nextNode() : null) as Text | null
          const htn = who?.firstChild as Text | null
          if (!who || !tn || !htn) {
            fail(`nothing to measure for label alignment ${where}`)
            return { v: 0, gap: 0 }
          }
          // Both measured as *text* boxes: the label carries its own padding and
          // the first paragraph sits inside block margins, so comparing element
          // boxes would compare chrome rather than what the eye aligns.
          const rng = document.createRange()
          rng.setStart(tn, 0)
          rng.setEnd(tn, 1)
          const line = rng.getBoundingClientRect()
          const hr = document.createRange()
          hr.setStart(htn, 0)
          hr.setEnd(htn, Math.min(4, htn.length))
          const head = hr.getBoundingClientRect()
          const v = line.top + line.height / 2 - (head.top + head.height / 2)
          // The other half of the complaint: the gutter. The live dot used to be an
          // inline box inside a 58px right-aligned nowrap label, which made the line
          // wider than its own box - and text-align does nothing to a line that
          // overflows, so the label fell back to the start edge and abutted the
          // answer's first word. Gap measured edge to edge, in the same frame.
          const gap = line.left - head.right
          pageLog(`label alignment (${where}): ${v.toFixed(1)}px off the line, ${gap.toFixed(1)}px gutter`)
          return { v, gap }
        }
        const mid = alignPx('while generating')
        // 6px is the design allowance: the label hangs 2px below the block top and
        // carries 3px of its own padding. More than that reads as a name that does
        // not line up with the sentence beside it.
        if (Math.abs(mid.v) > 6)
          fail(`the CWOFFEE label is ${mid.v.toFixed(1)}px off the first line while generating`)
        transcript?.apply({ kind: 'text', d: 'right now, and the label must not shift when they finish.' })
        transcript?.apply({ kind: 'turnEnd', d: { turn_id: 3, finish: 'stop' } })
        await new Promise((r) => setTimeout(r, 60))
        const after = alignPx('once settled')
        if (Math.abs(after.v - mid.v) > 1.2)
          fail(`the label moved ${(after.v - mid.v).toFixed(1)}px when the turn ended - it aligns only at the end`)
        // The gutter has to exist *while the dot is up*, not only after it. A label
        // that touches the answer's first word for the whole turn and gains its
        // margin at turn end is the bug, whatever the settled number looks like.
        if (mid.gap < 6)
          fail(`the label has ${mid.gap.toFixed(1)}px of gutter while generating - it is touching the answer`)
        if (Math.abs(after.gap - mid.gap) > 1.0)
          fail(
            `the gutter changed ${(after.gap - mid.gap).toFixed(1)}px at turn end (${mid.gap.toFixed(1)}px -> ${after.gap.toFixed(1)}px): the label is being re-laid-out by its own marker`,
          )

        // The reported case was an answer that opens with a heading, so measure
        // that too: heading line-height and margins differ from a paragraph's, and
        // the label hangs off the block, not off the first paragraph.
        transcript?.clear()
        transcript?.apply({ kind: 'turnStart', d: { turn_id: 4, model: 'selftest' } })
        transcript?.apply({ kind: 'text', d: '## The Copy\n\nThe photocopier in the basement started printing again ' })
        await new Promise((r) => requestAnimationFrame(() => setTimeout(r, 40)))
        transcript?.flushNow()
        const head1 = alignPx('heading answer, generating')
        if (document.querySelector('.blk-answer h2, .blk-answer h3, .blk-answer h1') === null)
          fail('an answer starting with `## ` did not become a heading while streaming')
        if (document.querySelector('.blk-answer h2, .blk-answer h3, .blk-answer h1') === null)
          fail('an answer starting with `## ` did not become a heading while streaming')
        // No absolute vertical bound for the heading case, deliberately: the label
        // hangs off the top of the block, and a heading carries top margin, so it
        // sits ~12px below the label by design. What must not happen is the label
        // *moving* - that was the bug - or losing its gutter next to a wide title.
        if (head1.gap < 6)
          fail(`the label has ${head1.gap.toFixed(1)}px of gutter next to a heading: it is touching the title`)
        transcript?.apply({ kind: 'turnEnd', d: { turn_id: 4, finish: 'stop' } })
        await new Promise((r) => setTimeout(r, 60))
        const head2 = alignPx('heading answer, settled')
        if (Math.abs(head2.gap - head1.gap) > 1.0 || Math.abs(head2.v - head1.v) > 1.2)
          fail('the label moves when a heading answer settles')
        pageLog(
          `label alignment ok (${mid.gap.toFixed(1)}px gutter generating / ${after.gap.toFixed(1)}px settled, heading ${head1.gap.toFixed(1)}px / ${head2.gap.toFixed(1)}px)`,
        )
        transcript?.clear()
      }

      // [2c] Emphasis arrives with the words, not at the blank line.
      //
      // The paragraph is still open when `**the copy**` closes, so the bold has
      // to be *appended inside* a <strong> created at the opener. Re-parsing the
      // paragraph to get the same pixels would rewrite spans the reader already
      // read, which is the one thing every blink report traces back to. So the
      // check measures the words on both sides of the construct: the ones before
      // must not move by a pixel, and the marker characters must never be
      // painted at all.
      {
        transcript?.clear()
        transcript?.apply({ kind: 'turnStart', d: { turn_id: 7, model: 'selftest' } })
        transcript?.apply({ kind: 'text', d: 'The photocopier printed ' })
        await new Promise((r) => requestAnimationFrame(() => setTimeout(r, 40)))
        transcript?.flushNow()
        const body = document.querySelector('.blk-answer .blk-body') as HTMLElement
        const para = body?.querySelector('.live-para') as HTMLElement | null
        const before = para?.lastElementChild as HTMLElement | null
        const bx = before?.getBoundingClientRect().left ?? 0
        if (!para || !before) fail('the streaming paragraph did not open')
        else {
          transcript?.apply({ kind: 'text', d: '**the copy** and `git status` quietly ' })
          await new Promise((r) => requestAnimationFrame(() => setTimeout(r, 40)))
          transcript?.flushNow()
          if (!para.isConnected) fail('the streaming paragraph was replaced when emphasis arrived')
          const strong = para.querySelector('strong') as HTMLElement | null
          const code = para.querySelector('code') as HTMLElement | null
          if (!strong) fail(`bold did not arrive while the paragraph was open: ${JSON.stringify(para.textContent)}`)
          else if ((strong.textContent ?? '').trim() !== 'the copy')
            fail(`bold wraps the wrong words: ${JSON.stringify(strong.textContent)}`)
          if (!code) fail('a code span did not arrive while the paragraph was open')
          const after = para.textContent ?? ''
          if (after.includes('**')) fail(`the emphasis markers are visible on screen: ${JSON.stringify(after)}`)
          if (after.includes('`')) fail(`the code markers are visible on screen: ${JSON.stringify(after)}`)
          // The words that were already on screen must be exactly where they were.
          const ax = before.getBoundingClientRect().left
          if (Math.abs(ax - bx) > 0.5)
            fail(`the word before the construct moved ${(ax - bx).toFixed(1)}px when the bold landed`)
          if (!before.isConnected) fail('a settled word span was detached by the inline pass')
          // Settling has to agree with the parser rather than fight it.
          transcript?.apply({ kind: 'text', d: 'downstairs.' })
          transcript?.apply({ kind: 'turnEnd', d: { turn_id: 7, finish: 'stop' } })
          await new Promise((r) => setTimeout(r, 80))
          const final = document.querySelector('.blk-answer .blk-body')
          const fs = final?.querySelector('strong')
          if (!fs || (fs.textContent ?? '').trim() !== 'the copy')
            fail(`the settled paragraph lost the bold: ${JSON.stringify(final?.textContent)}`)
          if (!final?.querySelector('code')) fail('the settled paragraph lost the code span')
          pageLog(
            `inline emphasis streams ok (bold and code appended in place, previous word moved ${(ax - bx).toFixed(1)}px, markers never painted)`,
          )
        }
        transcript?.clear()
      }

      // [2] Switching from thinking to answering drains the queue first.
      //
      // The streaming queue is joined and painted by whatever block kind is
      // *current at paint time*. So when the last thinking delta and the first
      // answer delta land in the same frame - which is exactly what a local model
      // does when it stops thinking - the thinking prose gets painted into the
      // answer, and the answer's own text is glued onto the end of it where no
      // blank line can ever separate it. A real turn produced precisely that: the
      // table sat correctly in the store, the thinking block was empty, the answer
      // began "The user wants just a table...", and no <table> was ever rendered.
      // Both directions are proven in this engine: with the drain removed this
      // case reports an empty thinking block, prose in the answer, and tables=0.
      {
        transcript?.clear()
        transcript?.apply({ kind: 'turnStart', d: { turn_id: 11, model: 'selftest' } })
        transcript?.addUser('Reply with ONLY a markdown table, no prose.')
        transcript?.apply({ kind: 'reasoning', d: 'The user wants just a table in markdown format.' })
        transcript?.apply({
          kind: 'text',
          d: '| name | value |\n|---|---|\n| alpha | 1 |\n| beta | 2 |',
        })
        await new Promise((r) => setTimeout(r, 120))
        transcript?.apply({ kind: 'turnEnd', d: { turn_id: 11, finish: 'stop' } })
        await new Promise((r) => setTimeout(r, 60))
        const prose = 'The user wants just a table'
        const think = document.querySelector('.blk-reasoning .blk-body')
        const ansBody = document.querySelector('.blk-answer .blk-body')
        if (!think || !(think.textContent ?? '').includes(prose))
          fail('the thinking prose left its thinking block when the answer started')
        else if (!ansBody || !ansBody.querySelector('table'))
          fail('a table that started in the same frame as the thinking did not become a table')
        else if ((ansBody.textContent ?? '').includes(prose))
          fail('the thinking prose was painted into the answer block')
        else
          pageLog('switch drain ok (thinking kept its words, the answer kept its table)')
        transcript?.clear()
      }

      // [3] The view follows the stream at a walk, never in jumps.
      //
      // Two failures, both measured off real frames. The old rule ("jump to the
      // bottom if it is within 160px") left a line sitting at one height for five
      // seconds and then moved it 112px, 92px, 46px. Pinning on every paint fixed
      // that only partly: a paint happens every 120ms, so the whole transcript
      // still moved 23-33px in a single frame - the user's own screen recording
      // shows 46px and 66px steps at 4.9s and 5.7s - and every word already read
      // teleports, which is the "blink" being reported here. So the follow runs on
      // the frame clock, and this check samples scrollTop on the frame clock too:
      // sampling at paint granularity is how the last version of this check passed
      // while the recording kept showing jumps.
      {
        transcript?.clear()
        transcript?.apply({ kind: 'turnStart', d: { turn_id: 3, model: 'selftest' } })
        const sc = document.querySelector('.scroller') as HTMLElement
        if (!sc) fail('no scroller for the follow check')
        const filler = Array.from(
          { length: 6 },
          (_, p) =>
            Array.from(
              { length: 5 },
              (_, i) =>
                `paragraph ${p} line ${i} of a long answer that keeps the transcript taller than the viewport.`,
            ).join('\n'),
        ).join('\n\n')
        let prevTop = sc.scrollTop
        let worstStep = 0
        let worstGap = 0
        let worstAccel = 0
        let prevStep = 0
        let frames = 0
        const jumps: string[] = []
        // Sample inside the same rAF chain that drives the feeding, one sample per
        // frame. A free-running sampler chain looks the same on the page and is not:
        // a pending frame can fire during a later await and score a scroll that the
        // check itself caused.
        const pump = () =>
          new Promise<void>((res) =>
            requestAnimationFrame(() => {
              frames++
              const step = Math.abs(sc.scrollTop - prevTop)
              const gap = sc.scrollHeight - sc.clientHeight - sc.scrollTop
              if (step > worstStep) worstStep = step
              if (Math.abs(step - prevStep) > worstAccel) worstAccel = Math.abs(step - prevStep)
              prevStep = step
              if (step > 12 && jumps.length < 5)
                jumps.push(`f${frames}: ${Math.round(prevTop)}->${Math.round(sc.scrollTop)} (gap ${Math.round(gap)})`)
              if (gap > worstGap) worstGap = gap
              prevTop = sc.scrollTop
              res()
            }),
          )
        for (let i = 0; i < filler.length; i += 96) {
          transcript?.apply({ kind: 'text', d: filler.slice(i, i + 96) })
          // ~250ms between arrivals: 96 characters is roughly what this endpoint
          // produces in that time (30-45 words/s). Feeding at eight times real
          // speed measures the harness's own queue, not the reader's experience.
          for (let f = 0; f < 15; f++) await pump()
          transcript?.flushNow()
          for (let f = 0; f < 3; f++) await pump()
        }
        // Let the glide finish, then judge where it ended up.
        for (let t = 0; t < 60; t++) {
          await pump()
          if (sc.scrollHeight - sc.clientHeight - sc.scrollTop <= 1) break
        }
        const away = sc.scrollHeight - sc.scrollTop - sc.clientHeight
        if (frames < 40) fail(`the follow check sampled ${frames} frames: it measured nothing`)
        // Half a line. A step is at most max(8px, 18% of the gap) and the gap is
        // bounded below, so anything past 12px means more than one step landed in
        // one frame - and every settled word on screen changed place at once, which
        // is what the user calls a blink.
        // Half a line steady, more only when the view has fallen behind a block
        // that landed all at once, and even then gathering speed frame by frame
        // (FOLLOW_ACCEL). A large step reached by a ramp is motion; a large step
        // that appears out of nowhere is the blink.
        if (worstStep > 24)
          fail(
            `the transcript moved ${Math.round(worstStep)}px in one frame: the follow lurches (${jumps.join(' | ')})`,
          )
        if (worstAccel > 8)
          fail(
            `the follow changed speed by ${Math.round(worstAccel)}px/frame in one frame: that is a jolt, not a glide`,
          )
        if (worstGap > 220)
          fail(`the newest line sat ${Math.round(worstGap)}px below the fold while streaming`)
        if (away > 4) fail(`the view finished ${Math.round(away)}px short of the bottom`)
        // Reading position is respected: scrolling up stops the follow, and the
        // text a reader stopped on does not move when a block commits under them.
        const maxScroll = sc.scrollHeight - sc.clientHeight
        if (maxScroll < 120)
          fail(`the transcript only scrolls ${maxScroll}px: the reader-up half of this check needs more filler`)
        sc.scrollTop = Math.max(0, sc.scrollTop - 220)
        await new Promise((r) => setTimeout(r, 80))
        if (sc.scrollTop > sc.scrollHeight - sc.clientHeight - 40)
          fail('the scroll-up that starts the reader-up half did not take effect')
        // Measure *text*, not an element box. The element version of this check
        // reported a 25px move that no eye could see: a body's box moves with
        // margin collapsing and with the box of whatever child lands first, while
        // the line a reader is actually looking at did not budge.
        const anchor = (() => {
          const mid = sc.getBoundingClientRect().top + sc.clientHeight / 2
          const walk = document.createTreeWalker(sc, NodeFilter.SHOW_TEXT)
          let best: Range | null = null
          let bestD = Infinity
          for (let n = walk.nextNode(); n; n = walk.nextNode()) {
            const t = n as Text
            if (!t.textContent?.trim()) continue
            const r = document.createRange()
            r.setStart(t, 0)
            r.setEnd(t, Math.min(8, t.length))
            const b = r.getBoundingClientRect()
            if (!b.height) continue
            const d = Math.abs(b.top - mid)
            if (d < bestD) {
              bestD = d
              best = r
            }
          }
          return best
        })()
        if (!anchor) fail('no settled text to anchor the scrolled-up reader to')
        const rng = anchor as Range
        const anchorTop = rng.getBoundingClientRect().top
        transcript?.apply({ kind: 'text', d: '\n\n## A committed heading ends the paragraph\n\n' })
        await new Promise((r) => requestAnimationFrame(() => setTimeout(r, 40)))
        transcript?.flushNow()
        if (!rng.startContainer.isConnected)
          fail("the line a reader had stopped on was replaced when a block committed below it")
        const moved = rng.getBoundingClientRect().top - anchorTop
        if (Math.abs(moved) > 1.5)
          fail(`committing a block moved the reader's line ${moved.toFixed(1)}px`)
        transcript?.apply({ kind: 'turnEnd', d: { turn_id: 3, finish: 'stop' } })
        await new Promise((r) => setTimeout(r, 40))
        transcript?.clear()
        pageLog(
          `stream follow ok (worst frame step ${Math.round(worstStep)}px, worst speed change ` +
            `${Math.round(worstAccel)}px/frame over ${frames} frames; worst ${Math.round(worstGap)}px below ` +
            `the fold, settled ${Math.round(away)}px off bottom, reader's line moved ${moved.toFixed(1)}px on commit)`,
        )
      }
      }

      // [4] Frame cadence while a real answer streams.
      //
      // Everything else in this harness measures what the page *draws*. This
      // measures whether the engine delivers the frames to draw on, which is the
      // axis a rendering-path change moves: WebKitGTK's zero-copy DMABUF renderer
      // is off by default in the app (gpu.rs) because it drops frames on this
      // box, and the copy path that replaced it has to be shown not to cost the
      // frame clock. Numbers for both modes live in docs/BENCHMARKS.md.
      {
        const cadBefore = await turnCount()
        // Through send(), not invoke(): a draft creates its row first, so the turn
        // lands in a session that exists. It is swept here rather than left to the
        // final sweep: an extra row in the sidebar changes which sessions a later
        // check opens, and one check's debris moved another check's counts.
        const cadFloor = (await invoke<Session[]>('list_sessions')).reduce(
          (m, s) => Math.max(m, s.id),
          0,
        )
        newSession()
        await invoke('set_reasoning', { level: 'off' })
        draft = 'Write about coffee roasting in exactly four paragraphs. Answer in the chat only, use no tools and create no files.'
        await send()
        const stop = startFrameCadence()
        const done = await waitTurnDone(cadBefore)
        const c = stop()
        await invoke('set_reasoning', { level: 'medium' })
        await sweepSelftest('after the frame-cadence turn', cadFloor)
        // The baseline is measured on the same page a moment later with nothing
        // streaming. Without it this check measures the machine: a box serving a
        // 50 GB model on nine cores cannot hand any page a 60 Hz clock, and the
        // failure text would blame the streaming path for the CPU situation.
        const idle = await idleFrameSample(900)
        // The numbers are logged whatever the verdict: the cost of the rendering
        // path is the reason for measuring it, and a check that hides its own
        // measurements when it fails cannot be compared between modes or machines.
        pageLog(
          `frame cadence: ${c.frames} frames over ${c.ms}ms while a turn streamed; ` +
            `median ${c.median.toFixed(1)}ms, p95 ${c.p95.toFixed(1)}ms, worst ${c.worst.toFixed(1)}ms ` +
            `(at +${Math.round(c.worstAt)}ms), off-screen spans ${c.hiddenSpans}, ` +
            `page long tasks ${c.longN} totalling ${Math.round(c.longMs)}ms`,
        )
        pageLog(
          `frame cadence baseline (nothing streaming): ${idle.frames} frames, ` +
            `median ${idle.median.toFixed(1)}ms, p95 ${idle.p95.toFixed(1)}ms`,
        )
        const starved = idle.median > 21
        if (offScreen()) {
          blindSkips++
          // A throttled frame clock is what an occluded page does, and this run
          // already proved the page was not laid out (the geometry notes above). The
          // numbers stay in the log; they are just not treated as a verdict.
          pageLog(`selftest note: skipped the frame-cadence verdict; ${offScreen()}`)
        } else if (c.frames < 60)
          fail(`frame cadence: only ${c.frames} animation frames in ${c.ms}ms: the sample is empty`)
        else if (starved) {
          // Absolute thresholds mean nothing when the page never gets 60 Hz idle. What
          // still has to hold is that streaming costs little more than idling: an
          // app-side regression shows up as the ratio, on any machine.
          pageLog(
            `selftest note: the machine is starved (idle median ${idle.median.toFixed(1)}ms), so the cadence is judged against the baseline rather than 60Hz`,
          )
          if (c.median > idle.median * 1.6 + 3)
            fail(
              `frame cadence: streaming runs at ${c.median.toFixed(1)}ms vs ${idle.median.toFixed(1)}ms idle - streaming costs the frame clock far more than it should, whatever the machine is doing`,
            )
          else if (c.worst > Math.max(250, idle.worst * 2))
            fail(
              `frame cadence: one gap of ${c.worst.toFixed(0)}ms while streaming (idle worst ${idle.worst.toFixed(0)}ms) - something blocked the frame clock`,
            )
          else pageLog(`frame cadence ok relative to the idle baseline (${idle.median.toFixed(1)}ms)`)
        } else if ((c.median > 21 || c.p95 > 45 || c.worst > 250) && c.longTasksMeasured && c.longMs < 60) {
          // The clock slowed, but the page did no long task while it did. Blaming the
          // app for frames the process was never given measures the machine, and the
          // idle baseline above says whether the machine was able to deliver them.
          pageLog(
            `selftest note: the frame clock slowed (median ${c.median.toFixed(1)}ms, p95 ${c.p95.toFixed(1)}ms, worst ${c.worst.toFixed(0)}ms) while the page itself ran ${c.longN} long task(s) totalling ${Math.round(c.longMs)}ms - nothing in the page accounts for it, so this is recorded, not blamed; the idle baseline was ${idle.median.toFixed(1)}ms`,
          )
        } else if (c.median > 21)
          fail(`frame cadence: median gap ${c.median.toFixed(1)}ms with ${Math.round(c.longMs)}ms of the page's own long tasks - the page is not getting a 60Hz clock, and its own work is part of the reason`)
        else if (c.p95 > 45)
          fail(`frame cadence: p95 gap ${c.p95.toFixed(1)}ms while an answer streams - that is a hitch, not a pause`)
        else if (c.worst > 250)
          fail(`frame cadence: one gap of ${c.worst.toFixed(0)}ms - something blocked the frame clock`)
        else pageLog('frame cadence ok')
        if (!done) pageLog('selftest note: the cadence turn did not finish; the sample above is partial')
      }

      // The rest of this stretch measures geometry on synthetic blocks, so the
      // transcript has to be in the document. A turn that never finished can leave the
      // view on a draft, and every rect would read 0.
      if (!(await ensureTranscriptMounted()))
        pageLog('selftest note: no session with messages could be opened, so layout checks will be skipped')

      // [5] An entrance may only ever run on a node that has never been painted.
      //
      // This is the property every reported "it blinks" lands on, so it is now
      // measured rather than trusted. Isolated in plain HTML inside this same
      // engine - no Svelte, no app code, real compositor frames at 120 fps - an
      // entry animation whose first keyframe is `opacity: 0` leaves its element
      // *fully unpainted* for the whole animation: 14 frames, about 120 ms, and
      // 17-23 frames for a longer opacity fade. A fresh node with no animation:
      // zero blank frames. Fill mode (`both`, `backwards`, `none`), blur,
      // transform and `will-change` change nothing. So the lesson is not "make
      // animations shorter" - it is that an entrance belongs to *newness*. Run
      // it on content the reader has already read and that content disappears
      // for the length of the animation, which is exactly the flash that was
      // reported over and over.
      //
      // The CSS deliberately binds entrances to structure rather than newness -
      // `.turn > *`, `.tin`, `.pop`, `.filechip`, `.toast`, `.empty` - which is
      // only safe while nothing re-creates those nodes. This check is what keeps
      // that safe: it watches every animation start in the page and fails if one
      // lands on a node that was already on screen, or runs a second time on the
      // same node.
      {
        transcript?.clear()
        const before = fails
        const born = new WeakMap<Element, number>()
        const started = new WeakMap<Element, Map<string, number>>()
        const seedAt = performance.now()
        for (const el of document.querySelectorAll('*')) born.set(el, seedAt)
        const mo = new MutationObserver((recs) => {
          const at = performance.now()
          for (const r of recs)
            for (const n of Array.from(r.addedNodes)) {
              if (n.nodeType !== 1) continue
              const el = n as Element
              if (!born.has(el)) born.set(el, at)
              for (const d of el.querySelectorAll('*')) if (!born.has(d)) born.set(d, at)
            }
        })
        mo.observe(document.body, { childList: true, subtree: true })

        const sig = (el: Element) => {
          const c = (el as HTMLElement).className
          const s = typeof c === 'string' ? c.trim().split(/\s+/)[0] : ''
          return el.tagName.toLowerCase() + (s ? `.${s}` : '')
        }
        const again: string[] = []      // same node, entrance run twice
        const late: string[] = []       // entrance on something already on screen
        const cut: string[] = []        // entrance cancelled: the node was thrown away
        const byName: Record<string, number> = {}
        let fresh = 0
        let oldest = 0
        const onStart = (e: AnimationEvent) => {
          const el = e.target as Element
          byName[e.animationName] = (byName[e.animationName] ?? 0) + 1
          const seen = started.get(el) ?? new Map<string, number>()
          const nth = (seen.get(e.animationName) ?? 0) + 1
          seen.set(e.animationName, nth)
          started.set(el, seen)
          if (nth > 1) {
            again.push(`${sig(el)} restarted ${e.animationName} (start #${nth})`)
            return
          }
          const at = born.get(el)
          if (at === undefined) {
            late.push(`${sig(el)} started ${e.animationName} although it predates this check`)
            return
          }
          const age = performance.now() - at
          if (age > oldest) oldest = age
          fresh++
          // A node created by the paint machinery starts its entrance on the
          // following frame; measured in the running app at 100-135 ms. An
          // entrance starting seconds after arrival is an entrance on content
          // that is sitting there already.
          if (age > 400)
            late.push(`${sig(el)} started ${e.animationName} ${Math.round(age)}ms after it arrived`)
        }
        const onCancel = (e: AnimationEvent) => {
          if (e.animationName === 'cwoffee-tin') cut.push(sig(e.target as Element))
        }
        document.addEventListener('animationstart', onStart, true)
        document.addEventListener('animationcancel', onCancel, true)

        // (a) a live turn: reveals, three kinds of structural commit, a tool card.
        transcript?.apply({ kind: 'turnStart', d: { turn_id: 91, model: 'selftest' } })
        const parts = [
          'Six parts of one streamed answer, so that a heading, a list and a table ',
          'all commit while the model is still talking.\n\n',
          '## Roast levels\n\n',
          '- light: cinnamon\n- medium: city\n\n',
          '| level | oil |\n|---|---|\n| light | none |\n\n',
          'A plain closing paragraph with no markup at all keeps its spans.\n\n',
        ]
        let turnEl: HTMLElement | null = null
        let kids = 0
        for (const d of parts) {
          transcript?.apply({ kind: 'text', d })
          await new Promise((r) => requestAnimationFrame(() => setTimeout(r, 60)))
          transcript?.flushNow()
          const t = document.querySelector('.turn') as HTMLElement | null
          if (!t) {
            fail('the live turn container vanished during the turn')
            break
          }
          if (turnEl && t !== turnEl) {
            fail(
              'the live turn container was replaced mid-turn: `.turn > *` would replay the ' +
                'entrance of every row in it at once, which is a whole-transcript flash',
            )
          } else if (turnEl && t.children.length < kids) {
            fail(`the turn lost a child (${kids} -> ${t.children.length}): blocks are being rebuilt, not appended`)
          }
          turnEl = t
          kids = t.children.length
        }
        transcript?.apply({ kind: 'toolCallBegin', d: { id: 'e1', name: 'exec' } })
        transcript?.apply({ kind: 'toolCallReady', d: { id: 'e1', args: { cmd: 'true' } } })
        transcript?.apply({ kind: 'toolExecEnd', d: { id: 'e1', ok: true, exit_code: 0, changes: [] } })
        transcript?.apply({ kind: 'note', d: { message: 'a note lands in the same turn' } })
        await new Promise((r) => requestAnimationFrame(() => setTimeout(r, 60)))
        transcript?.apply({ kind: 'turnEnd', d: { turn_id: 91, finish: 'stop' } })
        await new Promise((r) => setTimeout(r, 150))

        // (b) a popover: `.pop` carries `cwoffee-pop ... both`, so a re-render of
        // an open menu would fade the open menu out from under the pointer.
        const chip = document.querySelector('button.cbtn.chip[data-menu]') as HTMLElement | null
        if (!chip) fail('no composer chip to open a popover with')
        else {
          chip.click()
          await new Promise((r) => setTimeout(r, 260))
          const pop = document.querySelector('.pop') as HTMLElement | null
          if (!pop) fail('the popover did not open')
          else {
            // Same node, twice, with the menu contents swapped underneath it.
            pop.classList.toggle('pop-wide')
            pop.classList.toggle('pop-wide')
            await new Promise((r) => setTimeout(r, 200))
          }
        }

        // (c) a session round trip through the click path. Stored transcripts are
        // rendered outside `.turn` on purpose, so opening an old session must be
        // silent rather than play forty entrances at once.
        const rowMine = document.querySelector('.sessrow.active') as HTMLElement | null
        const rows = [...document.querySelectorAll('.sessrow')] as HTMLElement[]
        const rowOther = rows.find((r) => r !== rowMine)
        let roundTrip = false
        if (rowMine && rowOther) {
          roundTrip = true
          const openRow = (r: HTMLElement) =>
            (r.querySelector('.sessbtn') as HTMLElement | null)?.click()
          openRow(rowOther)
          await new Promise((r) => setTimeout(r, 500))
          openRow(rowMine)
          await new Promise((r) => setTimeout(r, 500))
        }
        if (chip) (document.querySelector('.reader') as HTMLElement | null)?.click()
        await new Promise((r) => setTimeout(r, 120))

        document.removeEventListener('animationstart', onStart, true)
        document.removeEventListener('animationcancel', onCancel, true)
        mo.disconnect()
        transcript?.clear()

        if (again.length)
          fail(`${again.length} entrance(s) restarted on the same node: ${again.slice(0, 3).join(' | ')}`)
        if (late.length)
          fail(
            `${late.length} entrance(s) ran on content already on screen, which hides it for the length ` +
              `of the animation: ${late.slice(0, 3).join(' | ')}`,
          )
        // A check that watches nothing passes vacuously, so require the traffic -
        // unless the platform asked for reduced motion, where these animations are
        // switched off by design and there is nothing to watch.
        const reduced = matchMedia('(prefers-reduced-motion: reduce)').matches
        const tins = byName['cwoffee-tin'] ?? 0
        const rises = byName['cwoffee-rise'] ?? 0
        if (!reduced && tins < 10) fail(`only ${tins} word reveals were seen: the replay watch proved nothing`)
        if (!reduced && rises < 3) fail(`only ${rises} block entrances were seen: the replay watch proved nothing`)
        if (!reduced && roundTrip && rises > 6)
          fail(`${rises} block entrances in one turn: rows are entering more than once per turn`)
        // Cancelling an entrance means the node was thrown away mid-animation. One
        // per committed list or table is the honest cost of committing markdown; a
        // cancel per paint means the transcript is being rebuilt under the reader.
        if (cut.length > 6)
          fail(`${cut.length} reveals were cancelled mid-flight: nodes are being rebuilt under the reader (${cut.slice(0, 4).join(', ')})`)
        // Only claim success on this check's own line if this check had no say in
        // the failure count: a line that says "ok" after a FAIL is the reason
        // people stop reading logs.
        if (fails === before)
          pageLog(
            `no entrance replays ok (${fresh} entrances on ${Object.keys(byName).length} animation kinds, ` +
              `${tins} reveals, oldest node at start ${Math.round(oldest)}ms, ${cut.length} cut short` +
              `${roundTrip ? ', session round trip silent' : ', no session switch available'})`,
          )
        // Verified by negative control, then removed: putting a word on screen, let
        // it settle for 520ms, and re-apply its entrance makes this fail with
        //   SELFTEST FAIL: 1 entrance(s) restarted on the same node:
        //     span.tin restarted cwoffee-tin (start #2)
        // A guard that has never been seen to fail is decoration.
      }

      // --- thinking sits under its own label; other blocks keep the gutter -----
      // The thinking label is a heading for the text under it: label, rule and body
      // edge share an x. Every other block hangs its label in the left gutter so the
      // reading column stays one straight edge. The two layouts are deliberate and
      // must not drift into each other.
      {
        // The premise is verified BEFORE the blocks exist. Verifying it after capturing
        // the nodes is worse than not verifying it: the recovery reopens a session,
        // that reopen repaints the transcript, and the captured nodes are then measured
        // while detached - which is how "expanded thinking block has no width: 0" came
        // back three runs in a row with the view demonstrably open.
        if (await blind('thinking and answer alignment')) {
          // counted skip; blind() logged why
        } else {
        transcript?.clear()
        transcript?.apply({ kind: 'turnStart', d: { turn_id: 1, model: 'selftest' } })
        transcript?.apply({ kind: 'reasoning', d: 'a thought worth reading, long enough to wrap a little.' })
        transcript?.apply({ kind: 'text', d: 'an answer in the normal column' })
        transcript?.apply({ kind: 'turnEnd', d: { turn_id: 1, finish: 'stop' } })
        await new Promise((r) => setTimeout(r, 80))
        const think = document.querySelector('.blk-reasoning') as HTMLElement
        const ans = document.querySelector('.blk-answer') as HTMLElement
        if (!think || !ans) fail('missing thinking or answer block to compare')
        else if (offScreen()) {
          fail(`the view moved under this check mid-measurement: ${offScreen()}`)
        } else {
          // A settled thinking body is collapsed by design, and a collapsed element
          // measures 0 - so open it before measuring it.
          const head = think.querySelector('.blk-head') as HTMLElement
          const body = think.querySelector('.blk-body') as HTMLElement
          head.click()
          await new Promise((r) => setTimeout(r, 300))
          const hr = head.getBoundingClientRect()
          const br = body.getBoundingClientRect()
          const pad = parseFloat(getComputedStyle(body).paddingLeft || '0')
          if (br.width < 100) {
            // A zero box has a cause, and the cause is in the ancestor chain: the
            // first entry marked [display:none] is the thing that hid it. Without this
            // line the failure reads as "CSS is broken" and costs three more runs.
            const chain = (el: Element | null) => {
              const out: string[] = []
              let n: Element | null = el
              while (n && out.length < 8) {
                const cs = getComputedStyle(n)
                const cls = String(n.className ?? '').trim().split(/\s+/).join('.')
                out.push(`${n.tagName.toLowerCase()}.${cls}${cs.display === 'none' ? '[display:none]' : ''}`)
                n = n.parentElement
              }
              return out.join(' < ')
            }
            const mounted = (document.querySelector('.transcript')?.contains(think) ?? false) ? 'under .transcript' : 'NOT under .transcript'
            pageLog(`selftest diag: thinking ${Math.round(br.width)}px, ${mounted}, chain=${chain(think)}`)
            fail(`expanded thinking block has no width: ${Math.round(br.width)}`)
          }
          if (Math.abs(hr.left - br.left) > 1.5)
            fail(`thinking label is not above its text: head ${Math.round(hr.left)} vs body ${Math.round(br.left)}`)
          // The rule is drawn at the body's left edge, so it lines up with the label.
          if (Math.abs(br.left - hr.left) > 1.5)
            fail(`thinking rule is not under the label: ${Math.round(br.left)} vs ${Math.round(hr.left)}`)
          if (pad < 8) fail(`thinking text is not tucked under the rule: ${pad}px`)
          const aHead = ans.querySelector('.blk-head')!.getBoundingClientRect()
          const aBody = ans.querySelector('.blk-body')!.getBoundingClientRect()
          if (aHead.width === 0 || aBody.width === 0)
            pageLog(
              `selftest diag: answer head ${Math.round(aHead.width)}px body ${Math.round(aBody.width)}px, ` +
                `under .transcript=${document.querySelector('.transcript')?.contains(ans) ?? false}, ` +
                `empty-mode=${document.documentElement.className.includes('empty-mode') || !!document.querySelector('.empty-mode')}`,
            )
          if (aBody.left - aHead.left < 40)
            fail(`answer label is no longer outdented: head ${Math.round(aHead.left)} body ${Math.round(aBody.left)}`)
          const label = think.querySelector('.blk-head')?.textContent ?? ''
          if (!/^Thought for /i.test(label.trim())) fail(`settled thinking reads "${label}"`)
        }
        transcript?.clear()
        pageLog('thinking aligns under its label ok')
        }
      }

      // --- the label does not crowd the answer while it streams -----------------
      // Reported as "the CWOFFEE tag sits too close to the text while it writes, and
      // it fixes itself at the end". The label lives in the gutter, so what the user
      // is describing is horizontal: the breathing room between the tag and the first
      // glyph. It has to be the same room while text arrives as after the markdown
      // pass, or the turn visibly shifts as it finishes.
      {
        transcript?.clear()
        transcript?.apply({ kind: 'turnStart', d: { turn_id: 1, model: 'selftest' } })
        const gap = () => {
          const blk = document.querySelector('.blk-answer') as HTMLElement
          if (!blk) return null
          const head = blk.querySelector('.blk-head')!.getBoundingClientRect()
          const body = blk.querySelector('.blk-body') as HTMLElement
          let first = body.querySelector('.tin') as HTMLElement | null
          let left = first ? first.getBoundingClientRect().left : NaN
          if (!isFinite(left)) {
            const w = document.createTreeWalker(body, NodeFilter.SHOW_TEXT)
            const n = w.nextNode()
            if (n) {
              const r = document.createRange()
              r.setStart(n, 0)
              r.setEnd(n, Math.min(1, (n.textContent ?? '').length))
              left = r.getBoundingClientRect().left
            }
          }
          if (!isFinite(left)) return null
          return { h: left - head.right, v: head.bottom - body.getBoundingClientRect().top }
        }
        const live: number[] = []
        if (await blind('the label gap mid-stream')) {
          // counted skip; the premise is checked before anything is painted
        } else
        for (const p of ['first streamed line, ', 'second streamed line arrives ', 'and a third ']) {
          transcript?.apply({ kind: 'text', d: p })
          await new Promise((r) => requestAnimationFrame(() => setTimeout(r, 40)))
          transcript?.flushNow()
          const g = gap()
          if (g === null) fail('no streamed text to measure the label gap against')
          else {
            live.push(g.h)
            if (g.h < 6) fail(`the label crowds the text mid-stream: ${Math.round(g.h)}px`)
          }
        }
        transcript?.apply({ kind: 'turnEnd', d: { turn_id: 1, finish: 'stop' } })
        await new Promise((r) => setTimeout(r, 150))
        const fin = gap()
        if (!live.length) {
          // Every mid-stream sample was skipped. Saying "holds" here would be the
          // harness congratulating itself on measuring nothing.
          blindSkips++
          pageLog('selftest note: skipped the label-gap drift claim; no mid-stream sample was measurable')
        } else if (fin === null) fail('no rendered text to measure after the turn')
        else {
          const worst = Math.max(...live) - Math.min(...live)
          if (worst > 3) fail(`the text edge moves during the turn: ${live.map((x) => Math.round(x)).join('px, ')}px`)
          if (Math.abs(fin.h - live[0]) > 3)
            fail(`the label gap changes at turn end: ${Math.round(live[0])}px -> ${Math.round(fin.h)}px`)
          pageLog(`label gap holds through the turn (${Math.round(fin.h)}px, drift ${Math.round(worst)}px)`)
        }
        transcript?.clear()
      }

      // --- a finished turn offers copy, share, and an honest branch button -----
      {
        transcript?.clear()
        transcript?.apply({ kind: 'turnStart', d: { turn_id: 1, model: 'selftest' } })
        transcript?.apply({ kind: 'text', d: 'the answer to copy' })
        transcript?.apply({ kind: 'turnEnd', d: { turn_id: 1, finish: 'stop' } })
        await new Promise((r) => setTimeout(r, 60))
        const acts = [...document.querySelectorAll('.turn-acts .tact')] as HTMLButtonElement[]
        if (acts.length !== 3) fail(`expected three turn actions, found ${acts.length}`)
        const copy = acts.find((a) => a.classList.contains('copy'))
        const share = acts.find((a) => a.classList.contains('share'))
        const branch = acts.find((a) => a.classList.contains('branch'))
        if (!copy || !share || !branch) fail('turn actions are mislabelled')
        else {
          if (!copy.querySelector('svg.tact-glyph')) fail('turn actions use a non-SVG icon')
          if (!branch.disabled)
            fail('branch is offered as if it worked; it does not exist yet')
          if (!/not built yet/i.test(branch.title))
            fail(`a disabled button should say why: ${branch.title}`)
          if (copy.disabled || share.disabled) fail('working actions are disabled')
          // Sharing answers with a sentence, not a chip. A share is something the
          // person did *with* the conversation; giving it a file chip put it in the
          // same row as the model's own output, and the next turn then treated the
          // export as a file it had made.
          const realShare = transcript?.onShare ?? null
          if (transcript) transcript.onShare = async () => 'saved to /tmp/selftest.md'
          share.click()
          await new Promise((r) => setTimeout(r, 150))
          if (document.querySelectorAll('.filechip').length)
            fail('a share left a file chip in the conversation')
          if (transcript) transcript.onShare = realShare
        }
        // A turn's files are that turn's. The report: a "clean up those downloads"
        // turn ended up wearing the files it had just deleted, because the strip
        // carried over and the next `addFiles` re-parented it.
        transcript?.clear()
        transcript?.apply({ kind: 'turnStart', d: { turn_id: 1, model: 'selftest' } })
        transcript?.apply({ kind: 'text', d: 'downloaded the sources' })
        transcript?.addFiles([{ path: 'metr_report.html', name: 'metr_report.html', mime: 'text/html', bytes: 430006 }])
        transcript?.apply({ kind: 'turnEnd', d: { turn_id: 1, finish: 'stop' } })
        await new Promise((r) => setTimeout(r, 60))
        transcript?.apply({ kind: 'turnStart', d: { turn_id: 2, model: 'selftest' } })
        transcript?.apply({ kind: 'text', d: 'Cleaned up. Deleted the files I created.' })
        transcript?.apply({ kind: 'turnEnd', d: { turn_id: 2, finish: 'stop' } })
        await new Promise((r) => setTimeout(r, 60))
        const turns = [...document.querySelectorAll('.turn')]
        if (turns.length !== 2) fail(`expected two turns for the file-ownership check, saw ${turns.length}`)
        else {
          const late = turns[1].querySelectorAll('.filechip')
          if (late.length)
            fail(
              `the cleanup turn is wearing ${late.length} file chip(s) from the turn before: ${Array.from(late).map((c) => c.textContent?.trim()).join(', ')}`,
            )
          const early = turns[0].querySelectorAll('.filechip')
          if (early.length !== 1) fail(`the turn that downloaded the file lost its own chip (${early.length})`)
        }
        transcript?.clear()
        pageLog('turn files ok (each turn owns its own files, and a share leaves no chip)')
      }

      // --- right-click gives app actions, not Back/Forward/Reload --------------
      {
        transcript?.clear()
        transcript?.apply({ kind: 'turnStart', d: { turn_id: 1, model: 'selftest' } })
        transcript?.apply({ kind: 'text', d: 'right click me' })
        transcript?.apply({ kind: 'turnEnd', d: { turn_id: 1, finish: 'stop' } })
        await new Promise((r) => setTimeout(r, 60))
        const target = document.querySelector('.blk-answer .blk-body') as HTMLElement
        if (!target) fail('no answer to right-click')
        else {
          const ev = new MouseEvent('contextmenu', {
            bubbles: true,
            cancelable: true,
            clientX: 120,
            clientY: 120,
          })
          target.dispatchEvent(ev)
          await new Promise((r) => setTimeout(r, 60))
          const menu = document.querySelector('.ctxmenu') as HTMLElement | null
          if (ev.defaultPrevented !== true) fail('the native context menu was not suppressed')
          if (!menu) fail('no app menu appeared')
          else {
            const labels = [...menu.querySelectorAll('.ctx-item')].map((b) =>
              b.textContent?.trim(),
            )
            if (!labels.some((l) => /Copy/.test(l ?? '')))
              fail(`the menu offers nothing useful: ${labels.join(' / ')}`)
            if (labels.some((l) => /reload|back|forward/i.test(l ?? '')))
              fail(`the menu kept the browser's items: ${labels.join(' / ')}`)
            const r = menu.getBoundingClientRect()
            if (r.right > window.innerWidth + 1 || r.bottom > window.innerHeight + 1)
              fail(`the menu hangs off the viewport: ${Math.round(r.right)}x${Math.round(r.bottom)}`)
            // Styling is part of the contract. A menu that forgot its CSS is a
            // full-width transparent strip of text at the bottom of the window,
            // which is what happened once when these rules never landed.
            const cs = getComputedStyle(menu)
            if (cs.position !== 'fixed') fail(`the menu is not fixed-positioned: ${cs.position}`)
            if (cs.backgroundColor === 'rgba(0, 0, 0, 0)' || cs.backgroundColor === 'transparent')
              fail('the menu has no background: it is unstyled')
            if (r.width > 340 || r.width < 120) fail(`the menu width is wrong: ${Math.round(r.width)}px`)
            document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
            await new Promise((rr) => setTimeout(rr, 40))
            if (document.querySelector('.ctxmenu')) fail('Escape does not close the menu')
          }
          // A field keeps the native menu: suppressing it would take away a Paste
          // entry that script cannot provide.
          const ta = document.querySelector('textarea') as HTMLElement
          if (ta) {
            const ev2 = new MouseEvent('contextmenu', { bubbles: true, cancelable: true })
            ta.dispatchEvent(ev2)
            if (ev2.defaultPrevented) fail('the composer lost its native menu')
          }
        }
        transcript?.clear()
        pageLog('context menu ok')
      }

      // --- the temp folder is "no workspace", not a folder ---------------------
      // Early builds wrote /tmp into sessions as the default. The sidebar drew it as
      // a group called `tmp` beside one called `No workspace`: the same thing twice,
      // neither of them true. It now buckets as Default, and registering it is
      // refused by the engine and said out loud in the UI.
      {
        if (!isTempRoot('/tmp') || !isTempRoot('/tmp/') || !isTempRoot(''))
          fail('the temp folder is not recognised as "no workspace"')
        if (!isTempRoot('/var/tmp')) fail('/var/tmp is the temp folder too')
        if (isTempRoot('/tmp/cwoffee-project'))
          fail('a folder inside the temp folder is a real project folder')

        const fake: Session = {
          id: 990_001,
          title: 'selftest temp bucket',
          workspace: '/tmp',
          model: '',
          created_at: 0,
          updated_at: 0,
          message_count: 2,
        }
        const saved = sessions
        sessions = [...saved, fake]
        const bucket = wsGroups.find((g) => g.path === '')
        if (!bucket || !bucket.list.some((s) => s.id === fake.id))
          fail('a session in /tmp is not bucketed under Default')
        if (wsGroups.some((g) => g.path === '/tmp'))
          fail('/tmp is drawn as a workspace group of its own')
        const labels = wsGroups.map((g) => g.label)
        if (bucket && bucket.label !== 'Default')
          fail(`the no-workspace group is labelled ${JSON.stringify(bucket.label)}, want "Default"`)
        sessions = saved

        await addFolderFor('/tmp')
        if (!toastText()) fail('adding the temp folder produced no message')
        else if (!/temp folder/i.test(toastText()))
          fail(`the refusal does not explain itself: ${JSON.stringify(toastText())}`)
        await loadWorkspaces()
        if (workspaces.some((w) => isTempRoot(w.path)))
          fail('the temp folder ended up registered as a workspace')
        toast = null
        pageLog('temp folder bucketed and refused ok')
      }

      // --- a session keeps the folder it started in ----------------------------
      // The engine refuses the move; the control has to look refused too, or every
      // session that cannot move invites a click that can only fail.
      {
        const home = hostState?.session_id ?? 0
        const real = sessions.find(
          (s) => !isBareDraft(s) && !isTempRoot(s.workspace) && !!s.workspace,
        )
        if (!real) {
          pageLog('no session with a real folder to test the workspace lock against')
        } else {
          if (!(await settleTurn('before the workspace lock check'))) fail('host would not go idle')
          await openSession(real.id)
          await refreshState()
          await new Promise((r) => setTimeout(r, 300))
          if (!wsLocked) fail(`session ${real.id} in ${real.workspace} is not reported as locked`)
          // `button.wsrow`, not `.wsrow`: the sidebar's workspace group header is also
          // a `.wsrow`, it comes first in the DOM, and a div has no `disabled`.
          const row = document.querySelector('button.wsrow') as HTMLButtonElement | null
          if (!row) fail('no workspace row in the composer')
          else if (!row.disabled) fail('the folder control is live for a session that has a folder')
          const before = hostState?.workspace ?? ''
          await chooseWorkspace()
          if ((hostState?.workspace ?? '') !== before)
            fail('a locked session still moved its workspace')
          if (!/keeps its folder/i.test(toastText()))
            fail(`the locked control did not say why it is locked: ${JSON.stringify(toastText())}`)
          toast = null
          newSession()
          await new Promise((r) => setTimeout(r, 200))
          if (wsLocked) fail('a fresh draft should be able to choose a folder')
          pageLog('workspace lock ok')
        }
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
        // Bare drafts are deliberately not in the sidebar, so the invariant is
        // "every session that the sidebar chooses to show is in a group". Comparing
        // against the full list made this fail whenever another check had left a
        // draft behind, which read as a grouping bug.
        const shown = sessions.filter((s) => !isBareDraft(s)).length
        if (inside !== shown)
          fail(`grouping holds ${inside} of ${shown} sessions the sidebar shows (${sessions.length} rows total)`)
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
      // A run that skipped most of its layout checks proved nothing about layout, and
      // silence about that would let "SELFTEST ok" mean something it does not.
      if (endpointSkips > 0) {
        pageLog(`selftest note: ${endpointSkips} turn-dependent check(s) were skipped because the endpoint was too slow to finish a turn`)
        if (endpointSkips >= 3)
          fail(
            `${endpointSkips} checks were skipped: the endpoint never completed a turn during this run (last measured prefill ${lastPrefill === null ? 'unknown' : lastPrefill.toFixed(1)} tok/s), so the turn-dependent behaviour above was not verified`,
          )
      }
      if (blindSkips > 0) {
        pageLog(`selftest note: ${blindSkips} geometry check(s) were skipped because the page was not being laid out`)
        if (blindSkips >= 4)
          fail(
            `${blindSkips} geometry checks were skipped: the window was off-screen for most of this run, so nothing about layout was verified. Keep the window visible and re-run.`,
          )
      }
      // Hand the host back to the conversation the user was in. The sweep deleted the
      // scratch sessions; leaving the host pointed at one of them meant the desktop
      // came back showing a harness title.
      if (session0) {
        await invoke('switch_session', { id: session0 }).catch((e) =>
          pageLog(`selftest could not return to session ${session0}: ${e}`),
        )
      }
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
      transcript.onToast = (msg, bad) => say(msg, !!bad)
      // WebKit's own right-click menu offers Back, Forward and Reload: three ways
      // to damage a frameless app window, and no way to do the things this app is
      // for. The menu is ours now - ctxmenu.ts records the one place the native
      // menu is deliberately left alone.
      installContextMenu({
        toast: (msg, bad) => say(msg, !!bad),
        quote: (text) => {
          draft = text.length > 4000 ? `${text.slice(0, 4000)}…` : text
          setTimeout(() => composerEl?.focus(), 0)
        },
        openFile: async (path) => {
          try {
            await invoke<string>('open_file', { path })
          } catch (e) {
            say(`could not open ${path}: ${e}`, true)
          }
        },
      })
      // "Share as Markdown" builds the document on the engine (the same code for
      // both transports) and then asks a person where it should go. The desktop
      // gets the native save dialog; a browser page cannot pick a folder, so the
      // browser does what it is good at and downloads. Either way the host's
      // workspace gains nothing, which is the point: an earlier version wrote into
      // `<workspace>/exports/`, and the next turn treated that file as its own
      // output - including "cleaning it up".
      transcript.onShare = async (session: number) => {
        if (!session) throw new Error('this conversation is not saved yet')
        const doc = await invoke<{ name: string; markdown: string }>('share_markdown', { session })
        if (!doc?.markdown) throw new Error('the host returned an empty document')
        if (transportKind === 'gateway') {
          const blob = new Blob([doc.markdown], { type: 'text/markdown;charset=utf-8' })
          const url = URL.createObjectURL(blob)
          const a = document.createElement('a')
          a.href = url
          a.download = doc.name
          document.body.append(a)
          a.click()
          a.remove()
          setTimeout(() => URL.revokeObjectURL(url), 5000)
          return `downloaded ${doc.name}`
        }
        const path = await invoke<string | null>('save_markdown_file', {
          name: doc.name,
          contents: doc.markdown,
        })
        return path ? `saved to ${path}` : 'not saved — you closed the dialog'
      }
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
      // The footer reports which search provider is live and whether the gateway is
    // up. Both were read only when their popover opened, so the toolbar showed
    // "unknown" until the user clicked it - a control that lies about the current
    // state until interrogated. Fetch them with everything else.
    const fetches: Promise<unknown>[] = [
      refreshState(),
      loadSessions(),
      loadModels(),
      loadWebSearch(),
    ]
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
      <!-- A tab strip, not a headline button. It is not wired to anything yet:
           Chat mode does not exist, and a control that lies about what the app can
           do is worse than one that is missing. New session keeps a key on the
           shortcut, a `+` here, and the `+` on each workspace row. -->
      <div class="flex items-center gap-1.5">
        <div class="modegrid flex-1" class:chat={modeTab === 'chat'}>
          <button
            class="modesel"
            role="tab"
            aria-selected={modeTab === 'cowork'}
            onclick={() => (modeTab = 'cowork')}
          >Cowork</button>
          <button
            class="modesel"
            role="tab"
            aria-selected={modeTab === 'chat'}
            onclick={() => (modeTab = 'chat')}
          >Chat</button>
        </div>
        <button class="newbtn" title="New session (Ctrl+N)" aria-label="New session" onclick={newSession}>
          <svg width="14" height="14" viewBox="0 0 14 14" aria-hidden="true"><path d="M7 2.6v8.8M2.6 7h8.8" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" fill="none"/></svg>
        </button>
      </div>

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
            title={transportKind === 'tauri'
              ? 'Add a folder to the sidebar (opens the folder picker)'
              : 'Add a folder on the host by path'}
            onclick={addWorkspaceFromHeader}
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
                      <span
                        class="sess-dot"
                        class:busy={runningSessions.has(s.id)}
                        aria-hidden="true"
                        title={runningSessions.has(s.id) ? 'answering' : ''}></span>
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
            <img class="cup" src="/icon.png" alt="" draggable="false" />
            <!-- Drawn in the artwork's own 256-unit space, so the feet of the
                 wisps stay on the coffee surface at every render size. -->
            <svg class="steamart" viewBox="0 0 256 256" aria-hidden="true">
              <defs>
                <!-- Ink at the rim, nothing at the top: a puff that thins out as it
                     rises reads as dispersing smoke instead of a moving line. -->
                <linearGradient id="cwoffee-steam-fade" x1="0" y1="1" x2="0" y2="0">
                  <!-- Soft at both ends: densest a little above the rim. An ink that is
                       strongest *at* the surface puts a smudge on the coffee itself. -->
                  <stop offset="0" stop-color="currentColor" stop-opacity="0.25" />
                  <stop offset="0.22" stop-color="currentColor" stop-opacity="0.95" />
                  <stop offset="0.6" stop-color="currentColor" stop-opacity="0.5" />
                  <stop offset="1" stop-color="currentColor" stop-opacity="0" />
                </linearGradient>
              </defs>
              <defs>
                <!-- Feet at y=56..60: the coffee surface in the artwork's own space (the
                     mouth spans y=52..78, the liquid x=62..178), so the steam starts
                     inside the cup and the anchoring survives any render size. -->
                <!-- Feet at y=50..52: the far edge of the coffee's surface in the
                     artwork's own space (the mouth spans y=52..78, the liquid x=62..178).
                     Starting inside the ellipse put ink on the drink, which read as a
                     smudge on the coffee rather than something leaving it. -->
                <path id="cwoffee-wisp-a" pathLength="100" d="M96 52 C86 38 104 28 96 10 C88 -8 106 -20 98 -40" />
                <path id="cwoffee-wisp-b" pathLength="100" d="M124 50 C114 32 134 20 126 0 C118 -20 138 -34 130 -58" />
                <path id="cwoffee-wisp-c" pathLength="100" d="M154 52 C146 36 164 26 156 8 C148 -8 166 -20 158 -42" />
              </defs>
              <!-- Each curve is drawn twice: a wide blurred body and a narrow brighter
                   thread inside it. One stroked curve reads as a wire; two read as
                   something soft with a hot centre, which is what smoke looks like. -->
              <g class="wisps">
                <use class="wisp body w1" href="#cwoffee-wisp-a" />
                <use class="wisp body w2" href="#cwoffee-wisp-b" />
                <use class="wisp body w3" href="#cwoffee-wisp-c" />
              </g>
              <g class="cores">
                <use class="wisp core c1" href="#cwoffee-wisp-a" />
                <use class="wisp core c2" href="#cwoffee-wisp-b" />
                <use class="wisp core c3" href="#cwoffee-wisp-c" />
              </g>
            </svg>
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
            <div
              class="text-[12px] font-semibold uppercase tracking-wide text-warn"
              title={approval.tool}
            >
              Approve · {riskHeadline(approval.tool)} ({approval.risk})
            </div>
            <!-- A scrollable region has to be focusable to be scrollable by keyboard.
                 The a11y rule assumes a non-interactive element cannot need focus,
                 which is exactly the case here, so it is ignored deliberately. -->
            <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
            <div
              class="appr-body mt-1 font-mono text-[13px]"
              role="region"
              aria-label="Command to approve"
              tabindex="0"
              bind:this={apprBodyEl}
            >
              {approval.summary}
            </div>
            {#if apprClipped}
              <div class="appr-more">the command continues — scroll this box</div>
            {/if}
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
                <svg width="15" height="15" viewBox="0 0 24 24" aria-hidden="true"><path d="m21.44 11.05-9.19 9.19a6 6 0 0 1-8.49-8.49l8.57-8.57A4 4 0 1 1 18 8.84l-8.59 8.57a2 2 0 0 1-2.83-2.83l8.49-8.48" stroke="currentColor" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round" fill="none"/></svg>
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

          {#if wsEditing && !wsLocked}
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
                    submitWsPath()
                  } else if (e.key === 'Escape') {
                    wsEditing = false
                    wsErr = ''
                  }
                }}
              />
              <button class="ws-act" onclick={submitWsPath}>set</button>
              <button class="ws-act" onclick={() => { wsEditing = false; wsErr = '' }}>cancel</button>
            </div>
          {:else}
            <button
              class="wsrow {wsLocked ? 'ws-locked' : ''}"
              disabled={wsLocked}
              title={wsLocked
                ? `This session works in ${hostState?.workspace ?? ''}. A session keeps its folder: start a new session to work somewhere else.`
                : isTauri()
                  ? 'Choose the directory the tools operate in'
                  : `Host workspace: ${hostState?.workspace ?? ''} - click to type a path`}
              onclick={chooseWorkspace}
            >
              <svg width="13" height="13" viewBox="0 0 14 14"><path d="M1.8 4.2h3.4l1 1.4h6v5.6a.8.8 0 0 1-.8.8H2.6a.8.8 0 0 1-.8-.8z" fill="none" stroke="currentColor" stroke-width="1.1"/></svg>
              <span>{wsChipLabel || 'Choose workspace'}</span>
              {#if wsBusy}<span class="ws-note">working…</span>
              {:else if wsLocked}<span class="ws-note">this session’s folder</span>
              {:else if viewId === null && draftWorkspace}<span class="ws-note">new session here</span>{/if}
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
        {@const wlabel = wrow ? wsLabelOf(wrow) : wp && !isTempRoot(wp) ? basenameOf(wp) : 'Default'}
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
      {:else if menu.kind === 'search'}
        <div class="pop-title">Web search</div>
        <div class="pop-item static">
          <span class="pop-lbl">In use now</span>
          <span class="pop-sub">{webSearch ? webSearch.describing : 'reading\u2026'}</span>
        </div>
        <div class="popseg">
          {#each SEARCH_PROVIDERS as p}
            <button
              class="pop-item {webSearch?.backend === p.id ? 'on' : ''}"
              role="menuitemradio"
              aria-checked={webSearch?.backend === p.id}
              disabled={webSearchBusy || !webSearch}
              onclick={() => void chooseWebSearch(p.id)}
            >
              <span class="pop-lbl">{p.label}</span>
              <span class="pop-sub">{p.sub}</span>
            </button>
          {/each}
        </div>
        {#if webSearch?.backend === 'searxng'}
          <label class="pop-item static field">
            <span class="pop-lbl">Instance URL</span>
            <input
              class="ws-input pop-field-input"
              type="text"
              placeholder="http://127.0.0.1:8080/search"
              bind:value={webSearchUrl}
              onchange={() => void chooseWebSearch('searxng')}
            />
            <span class="pop-sub">
              needs <span class="mono">format=json</span> enabled in its settings.yml
            </span>
          </label>
        {/if}
        {#if webSearch?.backend === 'auto'}
          <div class="pop-item static status">
            <span class="pop-lbl">Its own container</span>
            <span class="pop-sub">
              {webSearch?.auto?.note ?? 'checking\u2026'}
            </span>
            <span class="pop-sub mono s2">
              port {webSearch?.auto_port} · idle {webSearch?.auto_idle_secs}s · config {webSearch?.auto_dir}
            </span>
          </div>
          {#if !webSearch?.runtime}
            <div class="pop-note">
              No podman or docker on this machine, so Cwoffee cannot run an instance
              of its own. Search keeps working through DuckDuckGo and says so in every
              result until a runtime appears.
            </div>
          {/if}
        {/if}
        <button
          class="pop-item"
          role="menuitem"
          disabled={webSearchBusy}
          onclick={() => void runWebSearchProbe()}
        >
          <span class="pop-lbl">{webSearchBusy ? 'searching\u2026' : 'Try one search'}</span>
          <span class="pop-sub">
            {webSearchProbe ? `${webSearchProbe.results.length} result${webSearchProbe.results.length === 1 ? '' : 's'} in ${webSearchProbe.ms}ms` : 'asks the provider, not the model'}
          </span>
        </button>
        {#if webSearchProbe && !webSearchProbe.error}
          {#each webSearchProbe.results.slice(0, 3) as r}
            <div class="pop-item static">
              <span class="pop-lbl">{r.title}</span>
              <span class="pop-sub mono">{r.url}</span>
            </div>
          {/each}
          {#if webSearchProbe.note}
            <div class="pop-note">{webSearchProbe.note}</div>
          {/if}
        {/if}
        {#if webSearchErr}
          <div class="ws-err">{webSearchErr}</div>
        {/if}
        {#if webSearch?.env_override}
          <div class="pop-note">{webSearch.env_override}</div>
        {/if}
        {#if webSearch?.saved_to}
          <div class="pop-note">saved to {webSearch.saved_to}</div>
        {/if}
        <div class="pop-note">
          {transportKind !== 'tauri' ? 'the host\'s' : 'this machine\'s'}
          <span class="pop-mono">~/.config/cwoffee/config.toml</span> holds the choice,
          so it survives a restart. Turning this off never removes a container.
        </div>
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

    <button
      class="fbtn"
      data-menu
      aria-haspopup="menu"
      aria-expanded={menu?.kind === 'search'}
      aria-label="Web search"
      onclick={(e) => openMenu('search', e.currentTarget as HTMLElement)}
    >
      <span class="fdot {webSearchDot}" aria-hidden="true"></span>
      <span class="flbl">search</span>
      <span class="fsum">{webSearchText}</span>
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

  <!-- Refusals and outcomes that used to be invisible: a folder the engine declined
       while its panel was closed, a picker cancelled, a session whose folder cannot
       change. `role=status` so a screen reader hears it, and dismissible by hand. -->
  {#if toast}
    <div class="toast {toast.bad ? 'bad' : ''}" role="status" aria-live="polite">
      <span>{toast.text}</span>
      <button class="toast-x" aria-label="Dismiss" onclick={() => (toast = null)}>
        <svg width="10" height="10" viewBox="0 0 10 10"><path d="M2.6 2.6l4.8 4.8M7.4 2.6l-4.8 4.8" stroke="currentColor" stroke-width="1.4" fill="none" stroke-linecap="round"/></svg>
      </button>
    </div>
  {/if}
</div>
