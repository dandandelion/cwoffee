<script lang="ts">
  import { onMount, onDestroy } from 'svelte'
  // `tick` is already a name in this component (the status-polling interval), so Svelte's
  // own tick is aliased rather than shadowed: an await that resolves to a number is a bug
  // the compiler only caught because the names happened to collide.
  import { tick as renderTick } from 'svelte'
  import { invoke, Channel, transportKind, gatewayRole, onReconnect, onResync, revalidateSocket } from './lib/transport'
  import { getCurrentWindow } from '@tauri-apps/api/window'
  import type {
    ApprovalAnswer,
    EngineEvent,
    ExecStream,
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
  import { describeLoop, LoopWatch } from './lib/loop'
  import { LoopHold, IDLE_HOLD, aniFrames, armedFromUrl } from './lib/anipause'
  import { installContextMenu } from './lib/ctxmenu'
  import { pageLog } from './lib/pagelog'
  import Section from './lib/settings/Section.svelte'
  import Field from './lib/settings/Field.svelte'
  import Select from './lib/settings/Select.svelte'
  import Segmented from './lib/settings/Segmented.svelte'
  import Switch from './lib/settings/Switch.svelte'
  import Choices from './lib/settings/Choices.svelte'

  // ---- state --------------------------------------------------------------
  let rootEl: HTMLDivElement | undefined = $state()
  let transcript: Transcript | null = null
  /**
   * Where the reader is, as the transcript reports it: pinned means it will follow the newest
   * line, `away` is how far the bottom of the content sits below the view, and `ask` says a
   * question card is waiting. This is what the jump button is drawn from; the transcript owns it
   * because only the transcript knows which growth was work and which was the answer.
   */
  let followState = $state({ pinned: true, away: 0, ask: false })

  let busy = $state(false)
  let model = $state('')
  let draft = $state('')
  let mode = $state<'ask' | 'session' | 'auto'>('ask')
  let thinking = $state<ReasoningLevel>('xhigh')
  /** What the settings screen says a *new* conversation starts at. `thinking` is what this
    conversation is using; the two differ as soon as the composer dial moves, and the settings page
    has to show the first while the composer shows the second. */
  let thinkingDefault = $state<ReasoningLevel>('xhigh')
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
  let toast = $state<{ text: string; bad: boolean; note?: boolean } | null>(null)
  let toastTimer = 0
  /** Every app-level note this window has shown, oldest first. The checks read this
      instead of scraping the transcript, because notes are no longer in the transcript. */
  const noteLog: string[] = []
  /** Read the current toast text. Reading `toast` directly inside a long async
      block lets TS narrow the $state to `null` after an assignment and then call
      the next read `never`. */
  function toastText(): string {
    return (toast as { text: string } | null)?.text ?? ''
  }

  function say(text: string, bad = false, dwell = 4600) {
    toast = { text, bad }
    if (toastTimer) clearTimeout(toastTimer)
    toastTimer = setTimeout(() => (toast = null), dwell)
  }

  let noteStack = 0

  /**
   * An app-level note: a migration, a compaction, "this endpoint has no window set".
   *
   * These used to be painted into the transcript between the turns, and that is what made
   * the transcript messy: a reader scanning what the model had said kept hitting lines about
   * the software. They still deserve to be seen once, so they come here — with a longer dwell
   * than a refusal, because nobody has to act on them and 4.6s is how a notice becomes a
   * blur. A note that lands while one is already up is counted, not stacked: three migration
   * lines are one notice that says three arrived.
   */
  function noteToast(text: string) {
    noteLog.push(text)
    if (noteLog.length > 60) noteLog.shift()
    if (toast?.note) noteStack++
    else noteStack = 0
    toast = { text: noteStack ? `${text} (+${noteStack} earlier)` : text, bad: false, note: true }
    if (toastTimer) clearTimeout(toastTimer)
    toastTimer = setTimeout(() => {
      toast = null
      noteStack = 0
    }, 11000)
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
  type MenuKind =
    | 'mode' | 'model' | 'think' | 'row' | 'keys' | 'metrics' | 'gateway' | 'ws' | 'wspick' | 'search' | 'tools' | 'naming' | 'display'

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
    /**
     * Top edge of the pinned strip the opener lives in (the sidebar's identity row, for the
     * telemetry / gateway / theme buttons), or the opener's own top when it is not in a strip.
     * A panel that flips upward has to clear this, not just the pill: the pill sits
     * inside the bar with padding above it, and clearing only the pill left the
     * panel painted over the identity row's own text.
     */
    barTop: number
  }
  let menu = $state<Menu | null>(null)
  let menuPos = $state<{ x: number; y: number } | null>(null)
  let menuEl = $state<HTMLElement | null>(null)

  /* Settings got a screen because a popup is a decision you make once and then can
     never look at again. The same choices were spread over six popups, each of which
     closes the moment you use it, so "what search am I on?" meant opening three menus.
     The sections below are the popups' own bodies - one copy of each control, rendered
     into either surface - so a setting cannot mean two different things depending on
     where you reached it from. */
  // The display settings the host knows about, read from Rust (`prefs::UI_PREFS`) rather
  // than typed here: the list is the vocabulary, so a setting the app does not act on
  // cannot appear as a control, and a phone gets the same list the desktop does.
  interface UiPref {
    key: string
    label: string
    hint: string
    /** Which settings page renders it. A pref with no page is a control that exists nowhere. */
    group: string
    value: string
    default: string
    is_default: boolean
    values: string[]
  }
  let uiPrefs = $state<UiPref[]>([])
  let prefBusy = $state(false)

  async function loadPrefs() {
    try {
      uiPrefs = await invoke<UiPref[]>('ui_prefs')
      applyPrefs()
    } catch (e) {
      pageLog(`prefs: ${e}`)
    }
  }

  function applyPrefs() {
    transcript?.setVerbose(uiPrefs.find((p) => p.key === 'verbose_blocks')?.value === 'on')
  }

  async function setPref(key: string, value: string) {
    prefBusy = true
    try {
      const said = await invoke<string>('set_ui_pref', { key, value })
      toast = { text: said, bad: false }
      // Read the list back instead of painting what we sent: the store's answer is what
      // the screen should show, and a refused write would otherwise leave a number there
      // that was never stored.
      await loadPrefs()
    } catch (e) {
      toast = { text: `${e}`, bad: true }
      await loadPrefs()
    }
    prefBusy = false
  }

  /* Settings categories. Five names in the rail, because the rail's job is to be
     scanned, not read: what a section is for belongs at the top of the section,
     next to the controls it describes, not in a caption under a nav label.

     `title`/`desc` are what the page header shows; `label` is what the rail shows.
     Appearance and shortcuts share a page because both are facts about this window
     rather than about the model or the conversation, and neither carries enough to
     stand beside the other four. */
  type SetPage = 'models' | 'workspace' | 'devices' | 'conversation' | 'appearance'
  const SET_PAGES: { kind: SetPage; label: string; title: string; desc: string; icon: string }[] = [
    {
      kind: 'models',
      label: 'Models',
      title: 'Models',
      desc: 'Which model answers, which endpoint it comes from, how much it may think, and how much of the conversation it is allowed to carry.',
      icon: 'M4.5 6a1.5 1.5 0 0 1 1.5-1.5h4A1.5 1.5 0 0 1 11.5 6v4a1.5 1.5 0 0 1-1.5 1.5h-4A1.5 1.5 0 0 1 4.5 10zM6.5 2.5v2M9.5 2.5v2M6.5 11.5v2M9.5 11.5v2M2.5 6.5h2M2.5 9.5h2M11.5 6.5h2M11.5 9.5h2',
    },
    {
      kind: 'workspace',
      label: 'Workspace',
      title: 'Workspace',
      desc: 'What the coworker can do here, what it may do without asking, which folders its files may go in, and how it reaches the web.',
      icon: 'M2.5 5.6A1.6 1.6 0 0 1 4.1 4h2.5l1.2 1.7h5.6A1.6 1.6 0 0 1 13.5 7.3v4.1A1.6 1.6 0 0 1 11.9 13H4.1a1.6 1.6 0 0 1-1.6-1.6z',
    },
    {
      kind: 'devices',
      label: 'Devices',
      title: 'Devices',
      desc: 'Other windows, phones and machines attached to this app over the network, and what they are allowed to control.',
      icon: 'M2.5 4a1.5 1.5 0 0 1 1.5-1.5h7A1.5 1.5 0 0 1 12.5 4v4.5A1.5 1.5 0 0 1 11 10H4a1.5 1.5 0 0 1-1.5-1.5zM5.5 13h4M11.6 8.5h3.4a1 1 0 0 1 1 1v3.5a1 1 0 0 1-1 1h-3.4a1 1 0 0 1-1-1V9.5a1 1 0 0 1 1-1z',
    },
    {
      kind: 'conversation',
      label: 'Conversation',
      title: 'Conversation',
      desc: 'What a conversation is called, and what the one on screen is using: the context it carries, what the server already had, and how fast the answers came.',
      icon: 'M3 4.5h10a1 1 0 0 1 1 1v4.6a1 1 0 0 1-1 1H7.4l-2.6 2v-2H3a1 1 0 0 1-1-1V5.5a1 1 0 0 1 1-1z',
    },
    {
      kind: 'appearance',
      label: 'Appearance',
      title: 'Appearance & shortcuts',
      desc: 'How much of the working the transcript shows, the palette of this window, and the keys it answers.',
      icon: 'M2.5 5h11M2.5 8h11M2.5 11h6M11 5a1.5 1.5 0 1 0 0-.01M7 8a1.5 1.5 0 1 0 0-.01M12 11a1.5 1.5 0 1 0 0-.01',
    },
  ]

  /**
   * Where a name lives. A hash, a jump from another part of the app, and the app's
   * own page cases all name the thing they want - "search", "this session",
   * "thinking" - and each has to land on the page that now holds it, with that
   * section scrolled into view.
   *
   * The older names stay valid on purpose: a `#tools` link written down last month
   * still opens Workspace. A link that quietly stops landing is a feature that
   * broke without anyone announcing it.
   */
  const SECTION_HOME: Record<string, [SetPage, string]> = {
    models: ['models', 'answering'],
    model: ['models', 'endpoints'],
    endpoints: ['models', 'endpoints'],
    configured: ['models', 'endpoints-list'],
    think: ['models', 'reasoning'],
    reasoning: ['models', 'reasoning'],
    ceiling: ['models', 'ceiling'],
    context: ['models', 'ceiling'],
    workspace: ['workspace', 'tools'],
    tools: ['workspace', 'tools'],
    mode: ['workspace', 'approvals'],
    approvals: ['workspace', 'approvals'],
    ws: ['workspace', 'folders'],
    folders: ['workspace', 'folders'],
    search: ['workspace', 'search'],
    web: ['workspace', 'search'],
    devices: ['devices', 'gateway'],
    gateway: ['devices', 'gateway'],
    conversation: ['conversation', 'session'],
    naming: ['conversation', 'titles'],
    titles: ['conversation', 'titles'],
    metrics: ['conversation', 'session'],
    session: ['conversation', 'session'],
    appearance: ['appearance', 'transcript'],
    display: ['appearance', 'transcript'],
    transcript: ['appearance', 'transcript'],
    theme: ['appearance', 'theme'],
    keys: ['appearance', 'shortcuts'],
    shortcuts: ['appearance', 'shortcuts'],
  }
  // The tool list is read from the engine, never typed here: the description a tool
  // carries is the same text the model is handed, so a UI-side copy is a second copy of
  // prompt bytes that will drift.
  let toolViews = $state<{ name: string; description: string }[]>([])
  let toolsErr = $state('')
  async function loadTools() {
    try {
      toolViews = await invoke<{ name: string; description: string }[]>('tools_view')
      toolsErr = ''
    } catch (e) {
      toolsErr = String(e)
    }
  }
  /** Tool descriptions run long — they are written for a model. The screen shows the
      first sentence and the rest stays where it belongs, in the prompt. */
  function toolBlurb(d: string) {
    const first = d.split(/(?<=[.!?])\s/)[0] ?? d
    return first.length > 180 ? `${first.slice(0, 177)}...` : first
  }
  let settings = $state<{ page: SetPage; flash: string } | null>(null)
  let settingsEl = $state<HTMLElement | null>(null)
  let setBodyEl = $state<HTMLElement | null>(null)
  /** The app's own root, so the settings dialog can make everything else inert. */
  let shellEl = $state<HTMLElement | null>(null)

  /**
   * The size the settings dialog opens at, as the person left it.
   *
   * The dialog is destroyed when it closes, so a size dragged with `resize: both` would
   * otherwise be gone on the next opening - a control that resets every time you use it is a
   * control that does not exist. One size for the whole screen, not one per page: the pages
   * are tabs of the same dialog, and a frame that changes size when you change tab is the
   * behaviour that was complained about in the first place.
   */
  const SETTINGS_SIZE_KEY = 'cwoffee.settings.size.v1'
  let settingsSize = $state<{ w: number; h: number } | null>(readSettingsSize())

  function readSettingsSize(): { w: number; h: number } | null {
    try {
      const raw = localStorage.getItem(SETTINGS_SIZE_KEY)
      if (!raw) return null
      const v = JSON.parse(raw)
      if (typeof v?.w === 'number' && typeof v?.h === 'number' && v.w > 200 && v.h > 200) return v
      return null
    } catch {
      // A quota or a corrupt value is not worth an error message: the default size is fine.
      return null
    }
  }

  /** The dialog's inline size, if one was remembered. Clamped to the window on the way
   *  out, because a laptop unplugged from its monitor has fewer pixels than the last
   *  opening promised - and the CSS max-width/max-height are the second net. */
  function settingsStyle(): string {
    const s = settingsSize
    if (!s) return ''
    const w = Math.min(s.w, window.innerWidth - 16)
    const h = Math.min(s.h, window.innerHeight - 16)
    return `width:${w}px; height:${h}px`
  }

  /**
   * A `use:` action on the dialog: remember the size when it changes and again when it
   * goes away. The element fires `resize` when the person drags the corner (`resize: both`
   * in app.css is what makes that handle appear), which is the only moment the size stops
   * being the app's own.
   */
  function rememberSize(node: HTMLElement) {
    const save = () => {
      const w = node.offsetWidth
      const h = node.offsetHeight
      if (!w || !h) return
      settingsSize = { w, h }
      try {
        localStorage.setItem(SETTINGS_SIZE_KEY, JSON.stringify(settingsSize))
      } catch {
        /* private mode, quota: the size just does not persist */
      }
    }
    node.addEventListener('resize', save)
    return { destroy: save }
  }

  /**
   * While the settings dialog is open, the rest of the app is not a place you can act in.
   *
   * The dim was always painted over the toolbar, but painting over is not the same as being
   * unreachable: the toolbar's buttons stayed focusable, tabbable and - at 26% black -
   * indistinguishable from live controls. One report called it "clicks go through the
   * settings screen to the metrics bar"; whether the click landed on the dim or on the
   * button, the answer to the user was the same: the thing behind the dialog still seemed
   * to be theirs to press. `inert` is the browser's own word for that, and it covers what a
   * dim cannot: keyboard focus, tab order, and hit-testing. The dialog and its dim are the
   * two siblings left alone, because the dim is how you get out.
   */
  $effect(() => {
    const open = settings !== null
    if (!shellEl) return
    for (const kid of Array.from(shellEl.children)) {
      const el = kid as HTMLElement
      if (el === settingsEl || el.classList.contains('mscrim')) continue
      // `inert` is a property in current engines and nothing at all in older ones; the CSS
      // rule on `.app-shell.modal-open` is the half that works everywhere, so setting the
      // property can only add, never regress.
      try {
        ;(el as HTMLElement).inert = open
      } catch {
        /* no inert support here; the CSS rule is doing the work */
      }
      el.setAttribute('aria-hidden', open ? 'true' : 'false')
    }
  })

  const setPage = $derived(SET_PAGES.find((p) => p.kind === settings?.page) ?? SET_PAGES[0])
  const pageTitle = $derived(setPage.title)
  const pageDesc = $derived(setPage.desc)

  /** `what` is a name from SECTION_HOME - a category, a section, or one of the older
      names a link or another part of the app already uses - not an index. */
  function openSettings(what: string = 'models') {
    const home = SECTION_HOME[what]
    if (!home && !SET_PAGES.some((p) => p.kind === what)) {
      // An unknown name still opens the screen rather than doing nothing, but the log
      // says which name nothing answers: a dead link should be a visible bug, not a
      // panel that appears to ignore a click.
      pageLog(`settings: nothing answers "${what}"; opening ${SET_PAGES[0].kind}`)
    }
    const [page, section] = home ?? [what as SetPage, '']
    closeMenu()
    // The screen reads its own lists fresh, so another client's change is reflected
    // here the next time it is opened.
    void loadSettingsFor(page)
    settings = { page, flash: section }
    // Focus follows the surface: a screen nothing can tab into is a screen nothing can use.
    queueMicrotask(() => {
      settingsEl?.focus()
      void anchorSection(section)
    })
  }

  function gotoSettings(page: SetPage) {
    if (settings?.page === page) return
    openSettings(page)
  }

  /** Scroll the named section into the settings column and say, for a moment, that
      this is the one the link asked for. If nothing answers the name, the log says
      so: a deep link that lands somewhere plausible is worse than one that admits it
      missed. */
  async function anchorSection(section: string) {
    if (!section) return
    await renderTick()
    const el = setBodyEl?.querySelector<HTMLElement>(`[data-set-section="${section}"]`)
    if (!el) {
      pageLog(`settings: no section "${section}" on the ${settings?.page ?? 'closed'} page`)
      return
    }
    el.scrollIntoView({ block: 'start' })
    el.classList.add('flash')
    setTimeout(() => el.classList.remove('flash'), 1600)
  }

  function closeSettings() {
    settings = null
  }

  /* Settings is a place now, so a link can point at it: a phone opening
     http://host:8122/?token=…#settings:search should land on the section it asked
     for rather than on a transcript it did not ask for. Applied once, at boot, and
     the hash is left alone afterwards so reloading does not fight a hand that
     navigated away. `#settings`, `#settings:search` and a bare `#search` all work:
     people write these forms differently and none of them is wrong. */
  let deepLinked = false
  $effect(() => {
    if (deepLinked) return
    deepLinked = true
    const want = decodeURIComponent(location.hash.replace(/^#/, ''))
    if (!want) return
    const asked =
      want.startsWith('settings:') || want.startsWith('settings.')
        ? want.slice('settings.'.length)
        : want
    if (!asked || asked === 'settings') {
      openSettings()
      return
    }
    openSettings(asked)
  })
  // ----------------------------------------------------------------------------------
  // Providers and models (Settings > Model)
  //
  // The shape this screen is built around: one answering model, a list of endpoints, and
  // every endpoint expandable into the models on it. Three things the old popup could not
  // do and this one has to: keep more than one endpoint, tell you what an endpoint can
  // actually do before you commit to it, and let a model's window and sampling be set here
  // rather than in a config file you have to find by hand.
  // ----------------------------------------------------------------------------------
  type CapRow = {
    id: string
    provider?: string
    context_window?: number | null
    max_tokens?: number | null
    temperature?: number | null
    top_p?: number | null
    top_k?: number | null
    min_p?: number | null
    seed?: number | null
    efforts?: string[]
    supports_tools?: boolean | null
    supports_vision?: boolean | null
    source?: string
    configured?: boolean
    current?: boolean
  }
  type ProvRow = {
    name: string
    kind: string
    base_url: string
    has_key: boolean
    models: string[]
    current?: boolean
  }
  let provRows = $state<ProvRow[]>([])
  let capRows = $state<CapRow[]>([])
  let openProv = $state<Set<string>>(new Set())
  let openModel = $state<Set<string>>(new Set())
  let pvName = $state('')
  let pvUrl = $state('')
  let pvKey = $state('')
  let pvKind = $state('openai')
  let pvMsg = $state('')
  let pvBad = $state(false)
  let pvBusy = $state(false)
  let probing = $state(false)
  let probed = $state<CapRow[]>([])
  let mDraft = $state<Record<string, Record<string, string>>>({})

  /** Whether the endpoint text carries a `/v1` tail. Said out loud rather than silently
   *  normalised: on some servers `/v1` belongs in the base URL and on others the client adds
   *  it, and guessing wrong is a "connection refused" with no explanation. */
  const urlSaysV1 = $derived(/\/v\d*\/?$/.test(pvUrl.trim()))

  function draftFor(id: string): Record<string, string> {
    let d = mDraft[id]
    if (!d) {
      const c = capRows.find((x) => x.id === id)
      const num = (v?: number | null) => (v === null || v === undefined ? '' : String(v))
      d = {
        context_window: num(c?.context_window),
        max_tokens: num(c?.max_tokens),
        temperature: num(c?.temperature),
        top_p: num(c?.top_p),
        top_k: num(c?.top_k),
        min_p: num(c?.min_p),
        seed: num(c?.seed),
        efforts: (c?.efforts ?? []).join(','),
      }
      mDraft[id] = d
    }
    return d
  }

  async function loadProviders() {
    try {
      const st = await invoke<{
        chosen?: string
        chosen_base_url?: string
        providers?: ProvRow[]
        models?: CapRow[]
      }>('provider_status')
      provRows = st.providers ?? []
      capRows = st.models ?? []
      // Prefill the add form with what is answering now: most people opening this screen are
      // describing the box they already use, and an empty form makes them retype a URL.
      if (!pvUrl && !pvName) {
        const cur = provRows.find((x) => x.current)
        if (cur) {
          pvName = cur.name
          pvUrl = cur.base_url
          pvKind = cur.kind
        }
        // The endpoint most installs answer with is the one in the file's top-level section, and
        // that is not a registered row: the host says where the chosen model answers from, so the
        // form still describes something real when no rows exist yet.
        if (!pvUrl && st.chosen_base_url) pvUrl = st.chosen_base_url
      }
      // The endpoint answering now is worth having open: those are the models in use, and the
      // ones a person is most likely here to look at.
      for (const x of provRows) if (x.current) openProv = new Set([...openProv, x.name])
    } catch (e) {
      pvMsg = cleanErr(e)
      pvBad = true
    }
  }

  async function probeProvider() {
    if (probing) return
    probing = true
    probed = []
    pvMsg = 'probing — this asks the endpoint what each model can do, one small request per model'
    pvBad = false
    try {
      const out = await invoke<CapRow[]>('provider_probe', {
        base_url: pvUrl.trim(),
        api_key: pvKey,
        kind: pvKind,
        name: pvName.trim() || undefined,
      })
      probed = out ?? []
      pvMsg = probed.length
        ? `${probed.length} model(s) answered. Add the endpoint and they come with it.`
        : 'the endpoint answered but named no models'
      pvBad = probed.length === 0
    } catch (e) {
      pvMsg = cleanErr(e)
      pvBad = true
    } finally {
      probing = false
    }
  }

  async function testProvider() {
    if (pvBusy) return
    pvBusy = true
    pvMsg = 'asking the endpoint whether it is there…'
    pvBad = false
    try {
      const out = await invoke<string>('provider_test', {
        base_url: pvUrl.trim(),
        api_key: pvKey,
        kind: pvKind,
      })
      pvMsg = String(out)
      pvBad = false
    } catch (e) {
      pvMsg = cleanErr(e)
      pvBad = true
    } finally {
      pvBusy = false
    }
  }

  async function addProvider() {
    if (pvBusy) return
    if (!probed.length) {
      pvMsg = 'probe the endpoint first: adding an endpoint means naming the models on it, and a probe is what names them'
      pvBad = true
      return
    }
    pvBusy = true
    try {
      const out = await invoke<string>('provider_add', {
        base_url: pvUrl.trim(),
        api_key: pvKey,
        kind: pvKind,
        name: pvName.trim() || undefined,
        caps: probed,
      })
      pvMsg = String(out)
      pvBad = false
      probed = []
      await loadProviders()
      const nm = pvName.trim()
      if (nm) openProv = new Set([...openProv, nm])
    } catch (e) {
      pvMsg = cleanErr(e)
      pvBad = true
    } finally {
      pvBusy = false
    }
  }

  async function removeProvider(name: string) {
    try {
      const out = await invoke<string>('provider_remove', { name })
      pvMsg = String(out)
      pvBad = false
      await loadProviders()
      await refreshState()
    } catch (e) {
      pvMsg = cleanErr(e)
      pvBad = true
    }
  }

  function toggleProv(name: string) {
    const n = new Set(openProv)
    if (n.has(name)) n.delete(name)
    else n.add(name)
    openProv = n
  }

  function toggleModel(id: string) {
    const n = new Set(openModel)
    if (n.has(id)) n.delete(id)
    else n.add(id)
    openModel = n
    draftFor(id)
  }

  function parseField(v: string): number | null | 'bad' {
    const t = v.trim()
    if (!t) return null
    if (!/^-?\d+(\.\d+)?$/.test(t)) return 'bad'
    const n = Number(t)
    return Number.isFinite(n) ? n : 'bad'
  }

  /** Save one model's fields. Empty means "ask the endpoint", which the engine carries out by
   *  deleting the key — there is no number that means "unset", and the screen says so because
   *  people expect a blank field to have a value of its own. */
  async function saveModel(id: string) {
    const d = draftFor(id)
    const body: Record<string, unknown> = { id }
    for (const k of ['context_window', 'max_tokens', 'temperature', 'top_p', 'top_k', 'min_p', 'seed']) {
      const v = parseField(d[k] ?? '')
      if (v === 'bad') {
        pvMsg = `${id}: "${d[k]}" is not a number for ${k.replace('_', ' ')}`
        pvBad = true
        return
      }
      if (v !== null) body[k] = v
    }
    body.efforts = (d.efforts ?? '')
      .split(',')
      .map((x) => x.trim())
      .filter(Boolean)
    try {
      const out = await invoke<string>('model_set', body)
      pvMsg = String(out)
      pvBad = false
      await loadProviders()
      await refreshState()
    } catch (e) {
      pvMsg = cleanErr(e)
      pvBad = true
    }
  }

  async function forgetModel(id: string) {
    try {
      const out = await invoke<string>('model_forget', { id })
      pvMsg = String(out)
      pvBad = false
      delete mDraft[id]
      await loadProviders()
    } catch (e) {
      pvMsg = cleanErr(e)
      pvBad = true
    }
  }

  async function useModel(id: string) {
    try {
      await invoke('set_model', { model: id })
      await loadModels()
      await loadProviders()
      await refreshState()
      say(`answering with ${id}`)
    } catch (e) {
      pvMsg = cleanErr(e)
      pvBad = true
    }
  }

  /** The capability chips. Each one is an observation or an explicit "unknown": a model that
   *  was never probed must not be shown as text-only, and a model that refused tool calls must
   *  not be shown as unknown. */
  function capLabel(m: CapRow): string[] {
    const out: string[] = []
    // Short in the row, exact where it is edited: a chip for 262144 tokens pushes the
    // model's own name out of the row, and the field below shows the number in full.
    out.push(m.context_window ? `${fmtTokens(m.context_window)} context` : 'context unknown')
    if (m.supports_tools === true) out.push('tool calling')
    else if (m.supports_tools === false) out.push('no tool calling')
    else out.push('tool calling unknown')
    if (m.supports_vision === true) out.push('takes images')
    else if (m.supports_vision === false) out.push('text only')
    else out.push('images unknown')
    const eff = m.efforts ?? []
    out.push(eff.length ? `thinking: ${eff.join('/')}` : 'thinking unknown')
    if (m.source && !out.some((x) => x.startsWith('probed'))) out.push(m.source)
    return out
  }

  /// The reads each page needs, and nothing else. The same lazy rule the popups
  /// follow: a section never shows a state older than the last time anyone asked,
  /// and a container status probe (podman ps) has no business running before
  /// somebody opens Search.
  function loadSettingsFor(page: SetPage) {
    void loadPrefs()
    if (page === 'models') {
      if (modelErr || !models.length) void loadModels()
      void loadProviders()
    }
    if (page === 'workspace') {
      if (!toolViews.length) void loadTools()
      void loadWorkspaces()
      if (!webSearch) void loadWebSearch()
    }
    if (page === 'devices') void loadGateway()
    if (page === 'conversation') void refreshState()
  }

  /** Which folder rows are open on the Workspace page. Opening one is the moment to
      ask the store how many conversations live under that path - the visible sidebar
      list is not the folder's contents (it hides chat conversations and empty drafts),
      so the count comes from the store, and the delete label is only able to name a
      number after it has been fetched. */
  let wsOpen = $state<Set<string>>(new Set())
  async function toggleWsRow(path: string) {
    const next = new Set(wsOpen)
    if (next.has(path)) {
      next.delete(path)
      wsOpen = next
      return
    }
    next.add(path)
    wsOpen = next
    await armOne(path)
  }
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
  const THINK_LABEL: Record<ReasoningLevel, string> = {
    off: 'Off',
    low: 'Low',
    medium: 'Medium',
    high: 'High',
    xhigh: 'Extra high',
  }

  /* The option tables the settings controls draw. They live next to the state they
     change, not inside the markup: a choice and the sentence that explains what
     choosing it does belong together, and the same table answers the popup and the
     settings page. */
  const MODE_CHOICES = [
    {
      value: 'ask',
      label: 'Ask before anything changes',
      sub: 'Every call that writes, runs, or reaches outside the folder waits for an answer.',
    },
    {
      value: 'session',
      label: 'Ask once per tool in this conversation',
      sub: 'The first allow for a tool covers its later calls here, and nowhere else.',
    },
    {
      value: 'auto',
      label: 'Run without asking',
      sub: 'The denylist, the folder boundary and anything outside the folder are still enforced.',
    },
  ]
  const THINK_CHOICES = THINKS.map((t) => ({ value: t as string, label: THINK_LABEL[t] }))
  const SCOPE_CHOICES = [
    { value: 'loopback', label: 'This machine', title: '127.0.0.1: nothing else can reach the port' },
    { value: 'lan', label: 'Any device', title: '0.0.0.0: anything on the network can reach it, so the token is the only gate' },
  ]
  const PROVIDER_KINDS = [
    { value: 'openai', label: 'OpenAI-compatible' },
    { value: 'anthropic', label: 'Anthropic' },
    { value: 'llama.cpp', label: 'llama.cpp server' },
  ]

  const currentCapRow = $derived(
    capRows.find((x) => x.id === model || x.id.endsWith(`/${model}`)) ?? null,
  )
  const currentModelLine = $derived.by(() => {
    const p = provRows.find((x) => x.current)
    const bits: string[] = []
    if (p) bits.push(`${p.base_url} \u00b7 ${p.kind}`)
    bits.push(
      personaNow === 'chat'
        ? 'this conversation is in chat mode: no tools are mounted'
        : 'this conversation carries the tool set',
    )
    return bits.join('  \u00b7  ')
  })
  const urlHintLine = $derived(
    urlSaysV1
      ? 'The /v1 stays in the field on purpose: a llama.cpp-style server answers at /v1/chat/completions, and without it the app is one path short.'
      : 'No /v1 at the end. That is fine as a base URL: the app appends the path the chosen dialect uses. If this endpoint expects /v1 and it is left out, Test connection says so.',
  )
  /** What the endpoint said about the model the settings page is describing. "detected"
   *  is kept distinct from "stated": one is inferred from the served chat template. */
  function modelHint(m: ModelInfo): string {
    const b: string[] = []
    if (m.context_tokens) b.push(`${fmtTokens(m.context_tokens)} context`)
    const v = visionOf(m)
    if (v !== null) b.push(v ? 'takes images' : 'text only')
    if (m.tools !== null) b.push(m.tools ? 'tool calling' : 'no tool calling')
    if (m.slots) b.push(`${m.slots} slot${m.slots === 1 ? '' : 's'}`)
    if (m.tools_inferred) b.push('capabilities detected, not stated')
    else if (!m.advertised) b.push('capabilities not reported')
    return b.join('  \u00b7  ') || 'no capabilities reported'
  }
  const MODEL_CHOICES = $derived(
    models.map((m) => ({ value: m.id, label: m.id, hint: modelHint(m) })),
  )
  const thinkingNote = $derived.by(() => {
    const eff = currentCapRow?.efforts ?? []
    const on = THINK_LABEL[thinking]
    const here =
      hostState?.reasoning_overridden
        ? `This conversation is running at ${on}, a choice made in its own composer; it keeps that level for the rest of the conversation and no other conversation sees it.`
        : `This conversation is running at ${on}, which is the default.`
    const dflt = `New conversations start at ${THINK_LABEL[thinkingDefault]}.`
    // What the model process itself says it can answer for. This is the one sentence in the app that
    // answers "did the dial take effect?": a model that declares no thinking runs at off no matter
    // what the dial shows, and that used to be invisible - the level was clamped inside the model
    // process, before the request was built, and every surface still said `low`.
    const offered = hostState?.reasoning_levels ?? []
    const wire = !offered.length
      ? ' Nobody has asked the model process which levels it offers, so nothing here can promise this one reaches the wire.'
      : offered.includes(thinking)
        ? ` The model process says it answers for ${offered.join(', ')}, and this level is among them.`
        : ` The model process says it answers for ${offered.join(', ')} and not this one - the dial is set, but the level is not reaching the wire.`
    if (!eff.length)
      return `${here} ${dflt} No discovery has asked this endpoint which levels it accepts, so the request goes out and the server answers at whatever depth it has. Discover the model to see the levels it names back.${wire}`
    if (!eff.includes(thinking))
      return `${here} ${dflt} This endpoint named ${eff.join(', ')} and did not name this one, so the answer may come at the nearest level it implements.${wire}`
    return `${here} ${dflt} This endpoint named ${eff.join(', ')}. The level travels with each turn of this conversation; a model process that is already running keeps the level it started with.${wire}`
  })
  /** The settings page's own note: it edits the default, and the reader has to be able to tell that
    apart from the dial in the composer, which edits one conversation. */
  const thinkingDefaultNote = $derived.by(() => {
    const here = THINK_LABEL[thinking]
    const dflt = THINK_LABEL[thinkingDefault]
    return hostState?.reasoning_overridden
      ? `Every new conversation starts here. The one you are in has its own level (${here}), chosen in its composer, and it keeps that for the rest of the conversation - changing this does not change it.`
      : `Every new conversation starts here. The conversation you are in has not moved its own dial, so it follows this: ${here}.`
  })
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

  // A chat has no folder and no tools, so filing it under a workspace would make the
  // sidebar say something about it that is not true. Chats get their own list, newest
  // first, which is the order the store already returns them in.
  const isChat = (s: Session) => s.persona === 'chat'
  const chatSessions = $derived(sessions.filter(isChat))
  const wsGroups = $derived.by((): WsGroup[] => {
    const byPath = new Map<string, Session[]>()
    for (const s of sessions) {
      if (isBareDraft(s)) continue
      if (isChat(s)) continue
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
      // The word the person asked for. "Default" is the name of a setting, not of a place,
      // and it sits in a list of folder names - so a group of conversations that were never
      // given a folder read as if they had been given the app's chosen one. The chip, the
      // row menu, the model's own environment block and this header now all say the same
      // three words.
      out.push({ path, label: path ? basenameOf(path) : 'No workspace', list })
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
  /**
   * What the workspace chip says about a folder, in one function so the harness can ask it
   * directly instead of rebuilding the state a `$derived` needs.
   *
   * A conversation parked in the system's temp folder has no folder in the sense the person
   * means: nobody chose it, nothing of theirs is in it, and a reboot clears it. Drawing it as
   * `tmp` makes it look like a folder was picked, which is the report: "the backend workspace
   * is /tmp; the UI should say No workspace". The tools do run somewhere, and the app is not
   * going to pretend otherwise to the engine - but the word the app uses for that somewhere
   * has to be the one already used by the sidebar group and by the model's own environment
   * block, or the three disagree in front of the person.
   */
  function wsChipText(path: string): string {
    // No path at all is a different fact from a path in the temp folder, and it gets its own
    // words: a conversation that was never given a place says "no folder", while one parked
    // in the system's scratch folder says the phrase the sidebar and the model use.
    if (!path) return 'no folder'
    if (isTempRoot(path)) return 'No workspace'
    return basenameOf(path)
  }

  const wsChipLabel = $derived.by(() => {
    const path =
      viewId === null
        ? draftWorkspace || (hostState?.workspace ?? '')
        : (sessions.find((s) => s.id === viewId)?.workspace ?? '')
    if (viewId === null && draftWorkspace) return draftWsLabel
    return wsChipText(path)
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

  function foldAll() {
    wsCollapsed = new Set(wsGroups.map((g) => g.path))
    saveWsCollapsed()
  }

  function unfoldAll() {
    wsCollapsed = new Set()
    saveWsCollapsed()
  }

  /* A fold you have to redo every time you open the app is not a fold. The set of
     collapsed paths lives in localStorage (see WS_KEY) and is keyed by folder path, so
     it survives restarts. What must not survive is a preference for a folder that no
     longer exists: re-adding one would otherwise inherit a stale "closed". */
  let foldLogged = false

  function pruneWsCollapsed() {
    const live = new Set<string>(workspaces.map((w) => w.path))
    for (const s of sessions) live.add(wsBucket(s.workspace))
    let changed = false
    const next = new Set<string>()
    for (const p of wsCollapsed) {
      if (live.has(p)) next.add(p)
      else changed = true
    }
    if (changed) {
      wsCollapsed = next
      saveWsCollapsed()
    }
  }

  /* Selection mode, for when the sidebar gets cluttered. Nothing is selected until you
     ask for it: Ctrl/ Cmd-click adds a row to the basket, Shift-click paints the run
     between the last row and this one, and while the basket is open a plain click picks
     instead of navigating (a sidebar you cannot empty is a sidebar you stop using). The
     basket holds keys, not objects, so a pick cannot outlive the row it came from. */
  let picking = $state(false)
  let picked = $state<Set<string>>(new Set()) // 's<id>' = conversation, 'w<path>' = folder
  let pickAnchor = $state<string | null>(null)
  let bulkArm = $state(0)

  const pickedFolders = $derived([...picked].filter((k) => k[0] === 'w').map((k) => k.slice(1)))

  // A folder in the basket means it and what is inside it. Picking the row and leaving
  // twelve conversations behind is not deleting the folder, and the count on the confirm
  // step is the number that actually goes.
  const pickedSessions = $derived.by(() => {
    const ids = new Set<number>()
    for (const k of picked) {
      if (k[0] === 's') ids.add(Number(k.slice(1)))
      else {
        // By folder path over **every** loaded row, not over the group's visible list: a
        // folder group hides chat rows and empty drafts, and a pick that follows the visible
        // list deletes a folder's coworker conversations while its chats stay behind. The
        // folder then comes straight back — the sidebar builds its folder list from the paths
        // that hold sessions — and what the user sees is a Delete button that does not work.
        const want = k.slice(1)
        for (const s of sessions) if (wsBucket(s.workspace) === want) ids.add(s.id)
      }
    }
    return [...ids]
  })

  /**
   * What a folder actually contains, per the store.
   *
   * Two lists look like the answer and are not: `list_sessions` is a window of the newest 50,
   * and the sidebar's folder group deliberately hides chats and drafts. So a folder action
   * asks the store. If the store will not answer, this says so in the log and the caller
   * falls back to what it can see — a partial delete announced is a thousand times better
   * than a partial delete presented as success.
   */
  async function folderRows(path: string): Promise<Session[]> {
    try {
      return await invoke<Session[]>('sessions_in', { path })
    } catch (e) {
      pageLog(`sessions_in(${path}) failed: ${cleanErr(e)} — falling back to the loaded list`)
      return sessions.filter((x) => wsBucket(x.workspace) === path)
    }
  }

  /**
   * Delete rows by the same verb the row menu uses, one row at a time.
   *
   * A bulk delete that includes the conversation on screen has to unload it, or the
   * transcript keeps painting rows that no longer exist and the composer stays armed
   * against an id the engine can no longer load.
   */
  async function deleteRows(ids: number[]): Promise<{ gone: number; failed: number }> {
    let gone = 0
    let failed = 0
    for (const id of ids) {
      await invoke('delete_session', { id })
        .then(() => (gone += 1))
        .catch((e) => {
          failed++
          pageLog(`delete_session ${id} failed: ${e}`)
        })
    }
    if (viewId !== null && ids.includes(viewId)) showStartScreen()
    return { gone, failed }
  }

  /**
   * Forget a folder row, but only once nothing is inside it.
   *
   * `remove_workspace` deletes the registered path and nothing else, and the sidebar rebuilds
   * folder rows from any path that still holds sessions. Calling it while rows remain is not
   * a delete and not a removal: it is a no-op that looks like a failure. So the row stays, and
   * the sentence says why.
   */
  async function forgetFolder(path: string): Promise<{ removed: boolean; left: number }> {
    const left = (await folderRows(path)).length
    if (left > 0) return { removed: false, left }
    await invoke('remove_workspace', { path }).catch((e) => pageLog(`remove_workspace ${path}: ${e}`))
    return { removed: true, left: 0 }
  }

  const pickedRunning = $derived(pickedSessions.filter((id) => runningSessions.has(id)))

  function pickOrder(): string[] {
    const out: string[] = []
    for (const g of wsGroups) {
      out.push('w' + g.path)
      for (const s of g.list) out.push('s' + s.id)
    }
    return out
  }

  function pickToggle(key: string, range: boolean) {
    picking = true
    const order = pickOrder()
    const next = new Set(picked)
    const a = pickAnchor ? order.indexOf(pickAnchor) : -1
    const b = order.indexOf(key)
    if (range && a >= 0 && b >= 0) {
      for (let i = Math.min(a, b); i <= Math.max(a, b); i++) next.add(order[i])
    } else if (next.has(key)) next.delete(key)
    else next.add(key)
    picked = next
    pickAnchor = key
    // Any change to the basket invalidates the first press of the confirm.
    bulkArm = 0
  }

  function rowClick(e: MouseEvent, key: string, navigate: () => void) {
    if (e.ctrlKey || e.metaKey) {
      e.preventDefault()
      pickToggle(key, false)
      return
    }
    if (e.shiftKey) {
      e.preventDefault()
      pickToggle(key, true)
      return
    }
    if (picking) {
      pickToggle(key, false)
      return
    }
    navigate()
  }

  function clearPick() {
    picking = false
    picked = new Set()
    pickAnchor = null
    bulkArm = 0
  }

  /** Two presses, because a basket of folders is a pile of work, and the count named on
   *  the first press is the count the second press deletes. */
  async function deletePicked() {
    if (!picked.size) return
    // Ask the store what each picked folder holds before naming a number. The confirm step is
    // a promise about what the second press does, and the visible basket is a lower bound:
    // `list_sessions` returns the newest 50, and a folder group hides its chat rows.
    const byFolder = new Map<string, Session[]>()
    for (const p of pickedFolders) byFolder.set(p, await folderRows(p))
    const personaById = new Map<number, string>()
    const ids = new Set<number>(pickedSessions)
    for (const rows of byFolder.values())
      for (const r of rows) {
        ids.add(r.id)
        personaById.set(r.id, r.persona ?? 'cowork')
      }
    for (const x of sessions) if (!personaById.has(x.id)) personaById.set(x.id, x.persona ?? 'cowork')
    const all = [...ids]
    const doomed = all.filter((id) => !runningSessions.has(id))
    const skipped = all.filter((id) => runningSessions.has(id))
    const chats = all.filter((id) => personaById.get(id) === 'chat').length
    const busy = skipped.length
    if (!bulkArm) {
      bulkArm = Date.now()
      const bits = [`${doomed.length} conversation${doomed.length === 1 ? '' : 's'}`]
      // Chats are named because they are the rows the folder does not show: a count that
      // silently includes conversations the person cannot see next to the folder reads as the
      // app deleting more than it was asked to.
      if (chats) bits.push(`${chats} of them chat${chats === 1 ? '' : 's'}`)
      if (pickedFolders.length)
        bits.push(`${pickedFolders.length} folder${pickedFolders.length === 1 ? '' : 's'}`)
      if (busy) bits.push(`${busy} ${busy === 1 ? 'is' : 'are'} answering and will be skipped`)
      say(`delete ${bits.join(', ')} — press Delete again to confirm`, true)
      setTimeout(() => {
        if (bulkArm && Date.now() - bulkArm > 8000) bulkArm = 0
      }, 8100)
      return
    }
    const wasOpen = viewId !== null && doomed.includes(viewId)
    const res = await deleteRows(doomed)
    const failed = res.failed
    let removedFolders = 0
    const keptFolders: { path: string; left: number }[] = []
    for (const f of pickedFolders) {
      const r = await forgetFolder(f)
      if (r.removed) removedFolders++
      else keptFolders.push({ path: f, left: r.left })
      wsInside[f] = r.left
    }
    await loadSessions()
    await loadWorkspaces()
    await refreshState()
    if (wasOpen) {
      const rest = sessions.find((s) => s.id !== viewId)
      if (rest) openSession(rest.id)
      else newSession()
    }
    const bits = [`deleted ${res.gone} conversation(s)`]
    if (removedFolders) bits.push(`removed ${removedFolders} folder(s)`)
    if (skipped.length) bits.push(`${skipped.length} still answering and left alone`)
    if (failed) bits.push(`${failed} refused (log)`)
    say(bits.join(', '), failed > 0 || keptFolders.length > 0)
    if (keptFolders.length) {
      say(
        `${keptFolders.length} folder(s) stay: still holding ${keptFolders.reduce((a, k) => a + k.left, 0)} conversation(s) — a folder row is only dropped once nothing is inside it`,
        true,
      )
    }
    clearPick()
  }

  async function loadWorkspaces() {
    try {
      workspaces = await invoke<WorkspaceRow[]>('list_workspaces')
      pruneWsCollapsed()
      // Only the folders someone has already opened a menu on. A destructive label carries a
      // count, and a count that outlives the delete it described is a lie waiting for a click.
      for (const f of Object.keys(wsInside)) {
        if (!workspaces.some((w) => w.path === f)) {
          delete wsInside[f]
          continue
        }
        wsInside[f] = (await folderRows(f)).length
      }
      // Said once, in the log, because "it should have remembered" is not checkable
      // otherwise, and an empty count here is the difference between a lost preference
      // and one that was never set.
      if (!foldLogged) {
        foldLogged = true
        pageLog(`sidebar: ${wsCollapsed.size} folder(s) remembered closed, ${workspaces.length} folder row(s) known`)
      }
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

  /** How many conversations each folder holds, filled in when a folder menu opens.
   *  `-1` means "not asked yet", which the markup renders as a plain `Delete folder`. */
  let wsInside = $state<Record<string, number>>({})
  let wsDeleteArm = $state<string | null>(null)

  /** Ask the store how many conversations one folder holds, for the label of its delete item. */
  async function armOne(path: string) {
    wsInside[path] = (await folderRows(path)).length
  }

  /**
   * Two presses, and the second one only arms for this folder. A folder delete is a pile of
   * conversations going with it, and the label has to say the count on both presses.
   */
  async function deleteFolderAndRows(path: string) {
    menu = null
    const rows = await folderRows(path)
    if (!rows.length) {
      await removeFolder(path)
      say(`${basenameOf(path)}: no conversations inside, row dropped`)
      return
    }
    const running = rows.filter((r) => runningSessions.has(r.id))
    const doomed = rows.filter((r) => !runningSessions.has(r.id))
    const chats = rows.filter((r) => (r.persona ?? 'cowork') === 'chat').length
    if (wsDeleteArm !== path) {
      wsDeleteArm = path
      const bits = [`${doomed.length} conversation${doomed.length === 1 ? '' : 's'}`]
      if (chats) bits.push(`${chats} of them chat${chats === 1 ? '' : 's'}`)
      if (running.length) bits.push(`${running.length} answering ${running.length === 1 ? 'is' : 'are'} kept`)
      say(
        `delete ${basenameOf(path)} and ${bits.join(', ')} — press "Delete folder" again to confirm`,
        true,
      )
      setTimeout(() => {
        if (wsDeleteArm === path) wsDeleteArm = null
      }, 8100)
      return
    }
    wsDeleteArm = null
    // Rows for the message, ids for the verb: the same `delete_session` the row menu and the
    // basket use, because a second private idea of what deleting means is a second behaviour.
    const res = await deleteRows(doomed.map((r) => r.id))
    await loadSessions()
    const left = await forgetFolder(path)
    wsInside[path] = left.left
    await loadWorkspaces()
    await refreshState()
    if (!left.removed) {
      say(
        `${basenameOf(path)} stays: ${left.left} conversation(s) still filed here (${running.length} answering)`,
        true,
      )
    } else {
      say(
        `${basenameOf(path)} deleted (${res.gone} conversation(s)${res.failed ? `, ${res.failed} refused — see the log` : ''})`,
        res.failed > 0,
      )
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
    // The folder menu's destructive label carries a count, so opening the menu is the moment
    // to ask the store how many conversations live here. Until the answer lands the label says
    // the plain thing ("Delete folder") rather than a number nobody checked.
    if (wsPath) void armOne(wsPath)
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
      barTop: anchor.closest('.side-foot')?.getBoundingClientRect().top ?? r.top,
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
      // Flip above the opener - but above the *row* the opener sits in. Clearing
      // only the button left the panel's lower edge a few px inside the sidebar's pinned
      // footer, and the identity row showed through the panel's shadow.
      const above = Math.min(m.top, m.barTop) - h - 6
      y = above >= 8 ? above : Math.max(8, vh - h - 8)
    }
    menuPos = { x, y }
  })

  function closeMenu() {
    menu = null
    menuPos = null
    wsFilter = ''
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
    // The composer dial is the per-conversation ask. It must never write the config file, or a
    // level chosen for one chat becomes the level every future chat starts at.
    await setThinking(t, false)
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
  // IPC health is reported through render_report and the console. An empty catch here once hid a CSP
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
  // What closing this window would do to the gateway, from the host: which binary it would
  // start, or why it cannot. The "keep devices working" toggle is only honest next to that,
  // because the setting is a promise about a process that does not exist yet.
  interface GatewayPlan {
    binary: string | null
    running: boolean
    host: string
    port: number
    via: string
    stop: string
    log: string
  }
  let gwPlan = $state<GatewayPlan | null>(null)

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
  /**
   * The provider the panel is *offering*, whether or not the config accepted it yet.
   *
   * The URL box used to hang off the saved backend alone, which made a refused choice its
   * own cover story: pick "SearXNG, already running" with the box empty, the engine refuses
   * with "SearXNG needs a URL, for example http://127.0.0.1:8080/search", the saved backend
   * stays `auto`, and the field that message is asking for is the one that just stopped
   * rendering. The dead end is worse than the refusal. So visibility follows the offered
   * choice, and the choice only clears when the config takes it or the person picks
   * something else.
   */
  let webSearchPending = $state<string | null>(null)

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

  /** The backends the engine names, in the shape the radio rows draw: the engine's own
   *  label and the sentence it says about each one. */
  const SEARCH_CHOICES = $derived(
    SEARCH_PROVIDERS.map((p) => ({ value: p.id, label: p.label, sub: p.sub })),
  )

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
    // The offered choice is visible immediately, and stays visible if the write is refused:
    // "SearXNG needs a URL" has to come with the URL box, not with its disappearance.
    webSearchPending = id
    try {
      webSearch = await invoke<SearchView>('set_search_backend', {
        backend: id,
        // snake_case, like every other verb on this wire: the gateway reads these names
        // off the frame, and the desktop command now asks for them by the same spelling.
        searxng_url: webSearchUrl,
      })
      // Only a config that took the value gets to claim it. `webSearch.backend` is what the
      // file says, and the engine can answer with a different provider than the one asked
      // for (an unusable choice is refused and the old one kept).
      webSearchPending = webSearch.backend === id ? null : webSearch.backend
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

  /**
   * Which scope the Devices panel is offering, in one place. While a gateway is running the
   * answer is the address it is actually bound to; only when it is off does the panel get to
   * show what the next start would ask for. Both the control and the code that decides
   * whether a click has anything to do read this, so they cannot drift - and when they
   * drifted, one of the two scope buttons silently did nothing.
   */
  const gwScopeShown = $derived<'loopback' | 'lan'>(
    gw?.running ? (gw.host === '127.0.0.1' ? 'loopback' : 'lan') : gwScope,
  )

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
    try {
      gwPlan = await invoke<GatewayPlan>('gateway_handoff_plan')
    } catch (e) {
      // The panel still renders without it; what it would lose is the sentence that says
      // whether "keep devices working" can be honoured at all.
      gwPlan = null
      pageLog(`gateway hand-off plan: ${e}`)
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
      // The bind the app settled on is now the scope, whatever was asked for: a LAN bind
      // refused for want of a token leaves the gateway on loopback, and a panel still
      // showing "any device" over a 127.0.0.1 listener is how the next click goes wrong.
      if (gw?.running) gwScope = gw.host === '127.0.0.1' ? 'loopback' : 'lan'
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
    // Compare against what the panel *shows*, not against `gwScope`. The control's value
    // comes from the running listener whenever there is one, and `gwScope` is only what this
    // window last asked for. They disagree in the commonest case: a gateway that came back on
    // loopback (boot default, a refused LAN bind, another window) while this panel still said
    // "any device". Clicking "any device" then returned early, did nothing, and the only way
    // to reach the LAN was to click "this machine" first - twice - which is exactly what the
    // Devices page used to require.
    if (gwScopeShown === which) return
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
  // Paints delivered during the turn that just ended — the number the `turn end frames=` line
  // prints. A check that samples the DOM mid-stream is measuring the window manager when this
  // is 0: with no paints the incremental renderer writes nothing until the settle, so "the
  // bullets appeared all at once" says something about compositing, not about the app.
  let lastTurnFrames = 0

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

  /**
   * A conversation typing its own name.
   *
   * `titleDelta` arrives once per token, carrying the name so far; the row is not
   * renamed in the store until the name is final. So while a name is in flight the row
   * paints from here, and deliberately *not* by writing into `sessions`: a
   * `loadSessions()` landing mid-name would put "New Session" back on the row and the
   * animation would read as a glitch rather than as a name arriving.
   */
  let namingId = $state<number | null>(null)
  let namingText = $state('')
  /** When the first name paint for `namingId` arrived, for the ordering claim in the selftest. */
  let namingFirstAt = 0
  /** The final name the engine claimed for `namingId`, once it said it was done. */
  let namingDone = $state('')
  function namingOf(s: Session): string {
    return namingShownFor(s) ? namingText : s.title
  }
  /**
   * Whether the in-flight name may paint over this row's stored title.
   *
   * Two states, and the difference between them is a bug each:
   *  - the row still carries a placeholder: the name is arriving, so paint it;
   *  - the store has a real title: it wins, *unless* it is the same name the engine just
   *    finished claiming, in which case the caret gets its beat and the text is identical
   *    either way. Painting an in-flight name over a *different* stored title is how a rename
   *    somebody had just typed disappeared for a second and a half.
   */
  function namingShownFor(s: Session): boolean {
    if (namingId !== s.id || !namingText) return false
    if (isPlaceholderTitle(s.title)) return true
    return namingDone !== '' && s.title === namingDone
  }
  /** The names a conversation carries before anything has named it. */
  function isPlaceholderTitle(t: string): boolean {
    const x = (t || '').trim().toLowerCase()
    return x === '' || x === 'new session'
  }
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

  // The cup on the opening screen. Its picture carries its own idle motion now — an animated
  // WebP, whose frame table ends with a hold — so nothing in CSS moves it. That costs two things
  // the old CSS animation gave for free.
  //
  // One: `prefers-reduced-motion` can freeze a keyframe but cannot pause a raster loop. So the
  // reduced-motion case is a *different file* rather than the same file styled differently: frame
  // 0 of the same artwork, lifted out of the animation as a single-frame WebP, no re-encode and no
  // second generation of quality.
  //
  // Two: the pause baked into the file is a fixed pause, and a cup that rests for exactly 5.042s
  // every time reads as a clock. So the hold is rewritten per lap in memory — the last frame's
  // duration edited in a copy of the shipped bytes, three bytes and one object URL per cycle — and
  // drawn from IDLE_HOLD. The baked 5042ms stays as what plays before those bytes are in hand, and
  // as what keeps looping if the fetch ever fails.
  let reducedMotion = $state(false)
  const idleArt = $derived(reducedMotion ? '/cwoffee_idle_still.webp' : '/cwoffee_initial_idle_1.webp')
  let cupEl = $state<HTMLImageElement | null>(null)
  {
    const mq = matchMedia('(prefers-reduced-motion: reduce)')
    reducedMotion = mq.matches
    const sync = () => {
      reducedMotion = mq.matches
      // The cups in a transcript decide their own picture at paint time, so a change to the
      // setting has to reach them; waiting for the next turn would leave the conversation on
      // screen still moving (or still) after the user already asked it not to.
      transcript?.repaintCup()
    }
    mq.addEventListener('change', sync)
    onDestroy(() => mq.removeEventListener('change', sync))
  }
  $effect(() => {
    const el = cupEl
    if (!el || reducedMotion || !showEmpty) return
    const hold = new LoopHold(el, '/cwoffee_initial_idle_1.webp', IDLE_HOLD[0], IDLE_HOLD[1])
    return () => hold.stop()
  })
  /**
   * What the titlebar says after the wordmark: the conversation on screen.
   * Empty for a draft, where there is no conversation to name and the wordmark is
   * the whole title. Taken from the session list rather than from the host, because
   * the titlebar titles the *view* — which is not the session the engine is inside.
   */
  const viewTitle = $derived(
    viewId === null ? '' : sessions.find((s) => s.id === viewId)?.title ?? '',
  )
  let sentDraft: { text: string; atts: { mime: string; name: string; bytes: number }[] } | null = null
  // When this client sent the turn it believes is in flight, for `syncBusy`: a forced
  // reconciliation has to tell "that turn is a memory of one that ended while this tab
  // slept" from "that send has not reached the host yet".
  let turnSentAt = 0
  // A cap on *entries*, and entries are merged blocks, not tokens: see
  // `bufferLive`. 4000 raw deltas was the number that lost a turn's tail.
  const LIVE_CAP = 3000
  let liveDropped = 0

  /**
   * Add one event of a running turn to the replay buffer.
   *
   * Consecutive deltas of the same kind are the same text, so they merge into
   * one entry. Without that, a long turn - thinking plus a dozen tool loops -
   * emits thousands of deltas, hits the cap, and the buffer silently keeps the
   * *oldest* ones. Coming back to the session then showed the early loops and
   * nothing since, which is the report: "those loops and turns are lost".
   *
   * Merging is safe because the buffer is replayed into a cleared transcript:
   * the whole turn is rebuilt from the buffer, so one big delta and a thousand
   * small ones produce the same blocks.
   */
  function bufferLive(ev: EngineEvent) {
    // Live telemetry is not part of the answer. A replay rebuilds the transcript
    // and then gets the real numbers from `usage`/`turnEnd`, so buffering the
    // gauge - which arrives a few times a second, one per streaming burst - is
    // what used to eat the buffer while contributing nothing to it.
    if (ev.kind === 'turnStats') return
    const prev = liveBuf[liveBuf.length - 1]
    if (prev && prev.kind === ev.kind) {
      if (prev.kind === 'text' || prev.kind === 'reasoning' || prev.kind === 'toolCallArgs') {
        // Buffered events are not in the DOM: the view that showed them was
        // cleared before this buffer is read again.
        ;(prev.d as string) += ev.d as string
        return
      }
      if (prev.kind === 'toolExecOutput') {
        const a = prev.d as { id: string; stream: ExecStream; chunk: string }
        const b = ev.d as { id: string; stream: ExecStream; chunk: string }
        if (a.id === b.id && a.stream === b.stream) {
          a.chunk += b.chunk
          return
        }
      }
    }
    if (liveBuf.length >= LIVE_CAP) {
      // Loud rather than silent: a replay that is missing something is a bug
      // the log has to admit to.
      liveDropped++
      if (liveDropped === 1)
        pageLog(`live buffer full at ${LIVE_CAP} entries: this turn's later output is not replayable`)
      return
    }
    liveBuf.push(ev)
  }
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

    // Which conversation this event belongs to is decided *before* it is routed,
    // and from the engine's own words. `turnStart` names the session the turn
    // writes into; guessing from `viewId` is what let a phone on the gateway -
    // which sees every turn and started none of them - claim a turn as its own
    // and paint another conversation's thinking into the transcript on screen.
    const owner =
      ev.kind === 'turnStart' ? (ev.d.session_id ?? viewId ?? null) : turnSessionId

    // Live output belongs to the session that owns the turn. Buffer it when the
    // user is looking elsewhere, and replay it when they come back.
    //
    // An announcement about the app itself (a resumed history, a compaction, a
    // refusal) is not turn output and has no owner session to buffer against.
    // Treating it as turn output meant the note a session switch emits was parked
    // in the buffer during the repaint that same switch triggered - so it never
    // reached the screen at all.
    const announcement = ev.kind === 'note' || ev.kind === 'error'
    const inFlight = !announcement && owner !== null && ev.kind !== 'sessionChanged'
    if (inFlight) {
      // Buffer every event of the running turn, not just the ones that arrive
      // while the user is looking elsewhere. Buffering only the latter lost the
      // output that had already been painted before the switch, so coming back
      // showed an empty session - the very thing this is meant to prevent.
      bufferLive(ev)
      if (viewId !== owner || resyncing > 0) {
        // painted by the replay once the view settles
      } else {
        transcript?.apply(ev)
      }
    } else if (ev.kind === 'note') {
      // Stamped with the conversation the reader is looking at, so the note comes
      // back when they come back - and does not follow them somewhere else. With
      // no conversation open there is nothing to stamp it to; it stays where it
      // was painted.
      // The engine names the conversation the note is about; the client no longer
      // guesses it from what happens to be on screen. Stamping from `viewId` was how
      // the note announcing a switch got filed under the conversation the reader was
      // leaving, and then dropped by the repaint of the one they were entering.
      transcript?.showNote(
        ev.d.message,
        ev.d.session ?? viewId ?? undefined,
        ev.d.session == null || ev.d.session === viewId,
      )
    } else if (ev.kind === 'error') {
      // Painted now, and kept: an error that only exists in the view it happened in
      // is gone when the reader moves away and comes back, which is how a failed
      // turn gets remembered as a finished one.
      if (owner !== null) bufferLive(ev)
      transcript?.apply(ev)
    } else {
      transcript?.apply(ev)
    }

    switch (ev.kind) {
      case 'titleDelta': {
        // The name arriving, one paint per token. Cumulative by contract, so this is
        // a plain assignment: no append, no ordering hazard, no double-paint risk.
        const sid = ev.d.session_id
        if (namingId !== sid) namingFirstAt = performance.now()
        namingId = sid
        namingText = ev.d.text
        if (ev.d.done) {
          namingDone = ev.d.text
          // The caret outlives the last token by a beat. Dropped on the same frame as
          // the final character, the name reads as cut off mid-word.
          setTimeout(() => {
            if (namingId === sid) {
              namingId = null
              namingText = ''
              namingDone = ''
            }
          }, 1400)
        }
        break
      }
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
        // The engine named the conversation. Trust it over what this window
        // happens to be looking at - see the routing above.
        turnSessionId = owner
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
        // The tool stopped waiting: the person pressed Stop, a written-down clock ran
        // out, or the window holding the card closed. Left alone, the card looks
        // answerable forever; the `why` decides which sentence it ends with.
        transcript?.expireQuestion(ev.d.id, ev.d.why)
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
        // values the sidebar row's titles bind.
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
        lastTurnFrames = frames
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
  /**
   * The folder in prose rather than in a chip: a sentence needs the whole path, and needs
   * to say *none* when the path is the system's temp folder. `/tmp` in a sentence reads as
   * a folder somebody chose; it was not chosen, it is what a conversation with no folder
   * gets. The engine still runs its tools somewhere — that is not a lie to tell — but the
   * name the app uses for it is "no workspace", the same words the sidebar group and the
   * model's own environment block use.
   */
  const wsShownText = $derived(
    isTempRoot(hostState?.workspace)
      ? 'no workspace'
      : (hostState?.workspace ?? '').replace(/\/+$/, ''),
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
  async function retryAttach(force = false) {
    connNote = ''
    await attachChannel()
    if (connNote) return
    if (transportKind === 'gateway' && (await bootstrapGateway(force))) return
    await Promise.all([refreshState(force), loadSessions(), loadModels()])
    await adoptHostSession()
  }

  /**
   * The page is being looked at again after being hidden.
   *
   * On a phone this is where several "the app froze" reports live, and none of them
   * are a frozen app. Two things go wrong while the tab is backgrounded, and both are
   * still wrong when it comes back:
   *
   *   * the socket. iOS suspends the page, the gateway reaps the peer, and the browser
   *     never reports the close, so `readyState` goes on saying OPEN while every call
   *     hangs out its timeout. Nothing would notice until an RPC timed out twice.
   *   * `busy`. The turn's `turnEnd` event is simply gone, and this client believes the
   *     turn it started is still running, which keeps the composer `disabled` - the one
   *     state on the screen that stops a person typing.
   *
   * So: put the socket through a check it can fail, then re-read the host's state with
   * `force` so its answer beats this client's memory, then repaint the conversation on
   * screen so the answer that arrived while we were away is actually shown. Reload is
   * not an acceptable fix on a phone that was only multitasking.
   */
  let waking = false
  async function wakeCheck() {
    if (transportKind === 'tauri' || waking) return
    waking = true
    try {
      const { alive, silentMs } = revalidateSocket()
      pageLog(
        `page visible again: socket ${alive ? 'alive' : 'revalidating'} after ${
          Number.isFinite(silentMs) ? Math.round(silentMs / 1000) + 's silent' : 'no connection'
        }, busy=${busy} turnOurs=${turnOurs}`,
      )
      if (!alive) {
        // A reconnect is already armed, and the reconnected handler re-reads
        // everything. Don't start a second boot on top of it.
        return
      }
      const wasBusy = busy
      await refreshState(true)
      await loadSessions()
      // A turn that finished while the page slept left an answer in the store this view
      // never saw: its replay buffer only holds as much as arrived before the sleep, and
      // the `turnEnd` that would have settled it is gone. If nothing was in flight there
      // is nothing to repaint - a browser queues the frames it received while hidden and
      // delivers them on resume, so the transcript is already current, and repainting on
      // every tab switch would throw away where the reader had scrolled.
      if (wasBusy && !busy && viewId !== null) await resyncView(viewId)
    } catch (e) {
      pageLog(`wake check failed: ${e}`)
    } finally {
      waking = false
    }
  }

  function onVisibility() {
    if (document.visibilityState === 'visible') void wakeCheck()
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

  /**
   * The folder row on the greeting screen, and the list it opens.
   *
   * Before this, a session that had not been written showed a folder only *after* one was picked,
   * and the only way to pick one was a native file dialog launched from the composer's row. That is
   * two screens away from the decision, and it made the common case - "work in the folder I already
   * added" - into a browse. The picker puts the added folders in front of the composer with two
   * standing alternatives under them: add one, or choose none. The filter is kept while the panel is
   * open and dropped when it closes, because a list remembered with half a word typed in it is a
   * list that looks broken the next time it opens.
   */
  let wsFilter = $state('')

  /** The folders this machine knows, most recent activity first.
   *
   * The store returns them in whatever order its union of "registered" and "has sessions" comes back
   * in, which is not an order a person can predict. What they can predict is that the folder they were
   * working in yesterday is the one they are reaching for today. Nothing is hidden by the ranking - the
   * list is a fixed-height scroll box - the ranking only decides what is at the top.
   */
  const wsRanked = $derived.by(() => {
    const last = new Map<string, number>()
    for (const s of sessions) {
      if (!s.workspace) continue
      const prev = last.get(s.workspace) ?? 0
      if (s.updated_at > prev) last.set(s.workspace, s.updated_at)
    }
    return [...workspaces].sort((a, b) => {
      const la = last.get(a.path) ?? 0
      const lb = last.get(b.path) ?? 0
      if (la !== lb) return lb - la
      return wsLabelOf(a).localeCompare(wsLabelOf(b))
    })
  })

  /**
   * The exchange rule, stated inside the picker as well as on the chip.
   *
   * The chip in front of the composer is already disabled for a session that has a folder, so this
   * reads as redundant until the picker is open and the person is mid-ask. A row that looks clickable,
   * is clickable, and then refuses is a control that lies about what it does; a row that says it is
   * closed and why is a control that is still informing them. Both cores enforce the rule anyway -
   * the engine refuses `set_workspace` on a session that already has one - so this is the honest
   * surface of a rule that is already real.
   */
  const wsPickLocked = $derived(wsLocked)
  const wsPickLockNote = $derived(
    `this conversation already works in ${shortWorkspace}: a session keeps its folder, so start a new one to work somewhere else`,
  )

  /** True after the refusal was spoken, so the caller can bail without duplicating the sentence. */
  function refuseLockedPick(): boolean {
    if (!wsPickLocked) return false
    say(wsPickLockNote, true)
    closeMenu()
    return true
  }

  const wsFiltered = $derived.by(() => {
    const q = wsFilter.trim().toLowerCase()
    if (!q) return wsRanked
    return wsRanked.filter(
      (w) => wsLabelOf(w).toLowerCase().includes(q) || w.path.toLowerCase().includes(q),
    )
  })

  /**
   * The composer's folder chip: opens the picker, not the folder dialog.
   *
   * Choosing a workspace is mostly choosing among folders this machine already has, and a native dialog
   * cannot show that list - it can only show the filesystem. The dialog is still there, one more press
   * away under the seam, where asking for a *new* folder is the honest label for it. A session that has
   * already exchanged keeps its refusal, because a dead control has to say why it is dead.
   */
  function openWorkspacePicker(anchor: HTMLElement) {
    if (wsLocked) {
      void chooseWorkspace()
      return
    }
    wsFilter = ''
    openMenu('wspick', anchor)
  }

  function pickDraftWorkspace(path: string) {
    if (refuseLockedPick()) return
    applyDraftWorkspace(path)
    closeMenu()
  }

  /** "No workspace selected": the draft goes back to having no folder. */
  function clearDraftWorkspace() {
    if (refuseLockedPick()) return
    if (!draftWorkspace) {
      closeMenu()
      return
    }
    draftWorkspace = ''
    wsEditing = false
    wsDraft = ''
    wsErr = ''
    say('this session will have no folder: no tools, nothing written anywhere')
    closeMenu()
  }

  /**
   * "New workspace": add the folder to the list and put the draft in it, in that order.
   *
   * Adding is kept separate from applying everywhere else in this app, because a folder joining the
   * list must not move the tools out from under a session that is already running. Here it cannot:
   * the session has no folder yet, so "add it" and "work there" are one ask made by one press.
   */
  async function newWorkspaceFromPicker() {
    if (refuseLockedPick()) return
    if (transportKind !== 'tauri') {
      // There is no folder dialog on a machine you are not sitting at. The typed-path field is the
      // same ask, and it is the composer's, where the folder for a draft is edited already.
      closeMenu()
      wsDraft = draftWorkspace
      wsEditing = true
      return
    }
    wsAddErr = ''
    try {
      const picked = (await invoke('pick_workspace')) as string | null
      if (!picked) {
        say('no folder chosen')
        return
      }
      await addFolderFor(picked)
      if (wsAddErr) {
        say(`not added: ${wsAddErr}`, true)
        return
      }
      pickDraftWorkspace(picked)
    } catch (e) {
      wsAddErr = cleanErr(e)
      say(`not added: ${wsAddErr}`, true)
      pageLog(`pick_workspace from the workspace picker failed: ${String(e)}`)
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
    if (files.length) {
      e.preventDefault()
      void addFiles(files)
      return
    }
    // Nothing in the paste event. On this desktop that is normal for a
    // screenshot: WebKitGTK reports a paste with no file and no image item even
    // when the selection plainly holds a PNG, and there is no
    // navigator.clipboard.read() in an embedded webview either. So ask the shell
    // what the clipboard actually offers. An ordinary text paste is excluded
    // here - it is the common case and must not pay for a subprocess.
    const types = Array.from(e.clipboardData?.types ?? [])
    const imageish = types.some((t) => t.startsWith('image/') || t === 'text/uri-list')
    if (!imageish && types.includes('text/plain')) return
    void addClipboardImage()
  }

  /** Name for an image that arrived from the clipboard rather than a file. */
  function pastedName(mime: string) {
    const ext = (mime.split('/')[1] ?? 'png').replace('jpeg', 'jpg')
    const d = new Date()
    const pad = (n: number) => String(n).padStart(2, '0')
    return `clipboard-${pad(d.getHours())}${pad(d.getMinutes())}${pad(d.getSeconds())}.${ext}`
  }

  /**
   * Attach an image that is already base64 - the shape the shell hands back for a
   * clipboard image, and the shape the browser's Clipboard API produces.
   *
   * The same guardrails as addFiles apply, because an oversized or unsupported
   * image has to be refused in the composer with a reason rather than at the
   * engine boundary a second later.
   */
  async function addBase64Image(mime: string, bytes: number, b64: string, name: string) {
    attachErr = ''
    if (!OK_MIME.includes(mime)) {
      attachErr = `${name}: ${mime} is not a supported image`
      return false
    }
    if (bytes > MAX_ATTACH_BYTES) {
      attachErr = `${name}: ${fmtBytes(bytes)} exceeds ${fmtBytes(MAX_ATTACH_BYTES)}`
      return false
    }
    if (pending.length >= MAX_ATTACH) {
      attachErr = `at most ${MAX_ATTACH} images per message`
      return false
    }
    // A data: URL stands in for the blob URL a File would have produced. The
    // payload is already in memory; making a blob out of it would copy it again.
    pending = [...pending, { name, mime, bytes, b64, url: `data:${mime};base64,${b64}` }]
    return true
  }

  /**
   * Read an image off the system clipboard.
   *
   * The native path asks the shell (wl-paste / xclip), which is the only way a
   * screenshot taken outside this window can reach the composer. The browser path
   * uses the Clipboard API, which needs the user's gesture and shows a permission
   * prompt once - and says so when it is refused, rather than failing quietly.
   */
  async function addClipboardImage() {
    if (pending.length >= MAX_ATTACH) {
      attachErr = `at most ${MAX_ATTACH} images per message`
      return false
    }
    if (isTauri()) {
      try {
        const probe = await invoke<{
          image: { mime: string; bytes: number; data_b64: string; source: string } | null
          offered: string[]
          note: string
        }>('clipboard_image')
        if (!probe.image) {
          if (probe.note) attachErr = probe.note
          else pageLog(`clipboard offered [${probe.offered.join(', ')}]: nothing sendable`)
          return false
        }
        return await addBase64Image(
          probe.image.mime,
          probe.image.bytes,
          probe.image.data_b64,
          pastedName(probe.image.mime),
        )
      } catch (err) {
        attachErr = `clipboard image failed: ${String(err)}`
        return false
      }
    }
    try {
      const items = await navigator.clipboard.read()
      for (const it of items) {
        const type = it.types.find((t) => OK_MIME.includes(t))
        if (!type) continue
        const blob = await it.getType(type)
        const buf = new Uint8Array(await blob.arrayBuffer())
        // Chunked: String.fromCharCode(...buf) with a 2 MB image blows the stack,
        // which looks exactly like "paste does not work".
        let s = ''
        for (let i = 0; i < buf.length; i += 0x8000) {
          s += String.fromCharCode(...buf.subarray(i, i + 0x8000))
        }
        return await addBase64Image(type, buf.byteLength, btoa(s), pastedName(type))
      }
      pageLog('clipboard had no image the browser would hand over')
      return false
    } catch (err) {
      attachErr = `the browser would not let the page read the clipboard (${String(err)}); `
        + 'use Ctrl+V inside the window, or the image button'
      return false
    }
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
  /**
   * Ask for the window back, from inside the app.
   *
   * The layout and frame checks are only meaningful on a page the compositor is painting, and the
   * desktop's other applications take the window away mid-run. The app can present its own window;
   * anything outside it would have to fake input, which this desktop answers with a remote-desktop
   * consent dialog - and a test run must not be the reason that dialog appears. So the harness asks
   * whenever it finds itself blind. At component scope, because the frame sampler needs it as much
   * as the layout checks do.
   */
  let windowAsks = 0
  async function presentWindow(why: string) {
    if (transportKind !== 'tauri') return
    windowAsks++
    try {
      const said = await invoke<string>('present_selftest_window')
      pageLog(`selftest window (${why}, ask ${windowAsks}): ${said}`)
      // The host answers at once and does the window work on its own thread, because a window call
      // made from the IPC handler can park the handler - and the page's message channel with it.
      // So the ask has only been *started* when this returns; give the compositor a beat to answer
      // before the caller measures whether the window came back.
      await new Promise((r) => setTimeout(r, 350))
    } catch (e) {
      pageLog(`selftest window could not be presented (${why}): ${e}`)
    }
  }

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
    // The clock stopping is the symptom of an uncomposited window, and rAF cannot report it: when
    // there are no frames, there is no tick to notice the absence. So a timer watches the silence
    // and asks for the window back - Wayland honours an activate (focus) for a normal toplevel even
    // though it ignores "always on top", which is why the startup pin alone does not hold.
    let askedForWindow = false
    const watchdog = setInterval(() => {
      if (askedForWindow || performance.now() - last < 700) return
      askedForWindow = true
      void presentWindow('the frame clock went silent')
    }, 350)
    return () => {
      clearInterval(watchdog)
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
          title: 'New Session',
          workspace: draftWorkspace || null,
        })
        // A chat is not a conversation that happens to have no tools; it belongs to a
        // different list. The store defaults a row to cowork because that is true of most
        // rows, and the host creates rows in places that know nothing about personas — so
        // the one client that does know, the person who pressed the button while the tab
        // said Chat, says so here, before the first message lands.
        if (personaNow === 'chat') {
          await invoke('set_session_persona', { id, persona: 'chat' }).catch((e) =>
            pageLog(`set_session_persona: ${e}`),
          )
          // The sidebar lists chats out of the same rows it lists conversations from, so
          // recording the mode on the row is only half of it: the list on screen has to be
          // re-read, or the chat you just started is missing from the list that is for it.
          sessions = await invoke<Session[]>('list_sessions').catch(() => sessions)
        }
        viewId = id
        draftWorkspace = ''
        // The row exists now, so say so now. Without this the sidebar has no row for
        // the conversation you are typing into until the turn ends - no spinner, and a
        // titlebar that names nothing while the first question is already on screen.
        void loadSessions()
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
    turnSentAt = performance.now()
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
  function syncBusy(h: HostState, force = false) {
    // A turn this client started is trusted locally: the host flips `busy` a
    // command-queue hop after we send, and reading false in that window would
    // re-enable the composer mid-send.
    //
    // That trust has to expire, because `turnOurs` outlives the turn it describes
    // whenever the `turnEnd` event never arrives. A phone that backgrounds its tab
    // mid-turn is frozen by the OS; the socket dies with it and the event is gone for
    // good. `busy` then stays true forever, `disabled={busy}` keeps the composer dead,
    // and reloading the page is the only way out — which is exactly what was reported:
    // "on the phone I can't interact with any input, especially the composer; I always
    // have to refresh. The trigger seems to be minimising the browser and coming back."
    //
    // So a *forced* read - one taken because the page or the connection just came back
    // - takes the host's answer even about a turn we started. One exception stays
    // guarded: a send made a moment ago may still be crossing the network, and the host
    // is right to report it as not busy yet.
    if (turnOurs && (!force || (turnSentAt && performance.now() - turnSentAt < 2500))) return
    if (busy !== h.busy) {
      pageLog(`busy corrected ${busy}->${h.busy} from host state${force ? ' (forced)' : ''}`)
      // A stale `turnSessionId` misroutes the live buffer, so a client that
      // missed the end of a turn has to let go of it too.
      turnSessionId = null
      liveBuf = []
      busy = h.busy
      if (!h.busy) {
        // The host says nothing is running, so this client's claim that it started one
        // - and the copy of the message it typed, kept for repainting a partial turn -
        // are both memories of something that is over.
        turnOurs = false
        sentDraft = null
        turnStartTs = 0
      }
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

  // pi's compaction is the real thing: it asks the model to summarize the older turns and
  // keeps the recent ones verbatim. The note that follows quotes pi's own counts, so the
  // panel does not have to invent a number for what was saved.
  // The context ceiling, as a setting rather than a fact about one window. `budgetDraft`
  // is what is typed, not what is in force: the row shows the live number and says whether
  // it is pinned, so an empty box plus a placeholder is the honest "not typed anything yet".
  let budgetDraft = $state<number | ''>('')
  let budgetBusy = $state(false)
  let budgetMsg = $state('')

  async function setBudget(target: number | null) {
    if (budgetBusy) return
    budgetBusy = true
    budgetMsg = ''
    try {
      const said = await invoke<string>('set_context_budget', { target })
      budgetMsg = said
      say(said)
      // The host moves the meter on its own loop, so the command's reply can outrun the change.
      // One read here used to catch the pin still in place, and the row then kept saying "pinned by
      // you at <the number it had just stopped meaning>" until some later event refreshed the page.
      // So wait for the pair the host publishes together - the pin and the number - instead of
      // hoping one read lands after the other. Bounded: a refused ceiling will never land, and the
      // refusal text is the answer in that case.
      hostState = await invoke<HostState>('session_state')
      for (let i = 0; i < 12; i++) {
        const s = hostState
        const landed = target === null ? s.budget_pin == null : s.budget_pin === target
        if (landed) break
        await new Promise((r) => setTimeout(r, 100))
        hostState = await invoke<HostState>('session_state')
      }
      budgetDraft = ''
    } catch (e) {
      budgetMsg = String(e)
    } finally {
      budgetBusy = false
    }
  }

  function pinBudget() {
    const n = typeof budgetDraft === 'number' ? Math.trunc(budgetDraft) : NaN
    if (!Number.isFinite(n) || n <= 0) {
      budgetMsg = 'type a token count first: the ceiling is a number of tokens, not a feeling'
      return
    }
    void setBudget(n)
  }

  let compacting = $state(false)
  async function compactNow() {
    if (compacting) return
    compacting = true
    try {
      const note = await invoke<string>('compact', { instructions: null })
      pageLog(`compact: ${note}`)
      await refreshState()
    } catch (e) {
      pageLog(`compact: ${e}`)
    } finally {
      compacting = false
    }
  }

  async function refreshState(force = false) {
    // The pill says "on :8099 · 2 devices"; a value cached at boot would be a
    // guess within a minute. Cheap local read, so refresh it with the rest.
    void loadGateway()
    try {
      hostState = await invoke<HostState>('session_state')
      if (hostState) {
        mode = hostState.mode
        thinking = hostState.reasoning
        thinkingDefault = hostState.reasoning_default ?? thinking
        model = hostState.model
        syncBusy(hostState, force)
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
  async function bootstrapGateway(force = false): Promise<boolean> {
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
        thinkingDefault = hostState.reasoning_default ?? thinking
        model = hostState.model
        syncBusy(hostState, force)
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
        // The transcript owns buttons that name a conversation (export, branch), so it
        // needs the id of what it is holding. A boot that skipped this left them at 0,
        // and "Share as markdown" on a phone's first load answered "this conversation is
        // not saved yet" about a conversation that plainly was.
        if (transcript) transcript.sessionId = b.session_id
        transcript?.clear()
        transcript?.loadHistory(page.messages)
        transcript?.replayNotes(b.session_id)
        // A turn that is still running is the one thing this page cannot show on its
        // own: neither core has finished writing it. Same rule as `resyncView` - paint
        // the question only if the store has not already given it back, then replay what
        // this client saw before it lost the connection.
        if (turnSessionId === b.session_id) {
          if (sentDraft && !storeHoldsSentDraft(page.messages))
            transcript?.addUser(sentDraft.text, sentDraft.atts)
          // The one thing neither core has written to storage yet: the half of this turn's
          // answer that is still streaming. A page reloaded mid-answer used to paint the
          // reply starting at whatever arrived after the reload — the top of it simply gone,
          // and only the end of the turn put it back. The host keeps that open half in its
          // state, so the reloaded page can show what already happened and let the deltas
          // carry on underneath it.
          if (hostState?.busy && (hostState.open_text || hostState.open_reasoning))
            transcript?.seedOpen(hostState.open_text ?? '', hostState.open_reasoning ?? '')
          for (const e of liveBuf) transcript?.apply(e)
        }
        viewMsgCount = turnSessionId === b.session_id ? Math.max(page.messages.length, 1) : page.messages.length
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
    // Looking at the conversation already on screen is a no-op - but only when it is actually
    // on screen. A view can be *named* (viewId set, row count known from the host) while its
    // transcript never painted; bailing out here left that state unfixable, because the one
    // thing a person would try - clicking the row again - did nothing at all.
    if (id === viewId && document.querySelectorAll('.transcript .turn').length > 0) return
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
    // A repaint of the conversation already on screen has to hand back the window
    // the reader is looking at, not the default page. Loading two pages of history
    // and then having the host announce a session switch that names the same
    // conversation used to snap the transcript back to the last 80 rows: the rows
    // the user asked for were there, then they were not, which is what "Load
    // history disappears" looks like from the chair. Switching *to* a different
    // conversation starts a fresh window instead, so a big view of one session does
    // not inflate the first paint of another.
    const keepRows = viewId === id ? viewMsgCount : 0
    viewId = id
    // The transcript owns buttons that name a conversation (export), so it needs
    // the id of what is on screen - which is not the same as the session the
    // engine happens to be working on.
    if (transcript) transcript.sessionId = id
    transcript?.clear()
    let rows: StoredMessage[] = []
    try {
      rows = await loadTranscript(id, keepRows)
    } catch (e) {
      pageLog(`session_messages failed: ${e}`)
    }
    resyncing--
    // A newer resync has already taken the view; painting this one's result
    // would show a session the user has since moved away from.
    if (mySeq !== resyncSeq) return
    // Returning to the session that owns a running turn: the store does not
    // hold that turn yet (the host persists a turn when it ends), so what is
    // replayed here comes from the buffer. `sentDraft` only decides whether this
    // client can also paint the question it typed - a phone watching a turn it
    // did not send still has to show the answer it watched, which is the whole
    // reason the buffer exists.
    if (turnSessionId === id) {
      // Paint the question this client typed only if the store has not already given
      // it back. See `storeHoldsSentDraft`.
      if (sentDraft && !storeHoldsSentDraft(rows))
        transcript?.addUser(sentDraft.text, sentDraft.atts)
      for (const e of liveBuf) transcript?.apply(e)
    }
    await refreshState()
  }

  /**
   * Does the page just loaded from the store already contain the message this client
   * sent for the turn in flight?
   *
   * The two cores answer that at different moments, which is why the answer cannot be
   * hardcoded. The rust core persists a turn when it ends, so while one runs the store
   * has no row for the sentence you just typed and the view has to supply its own copy.
   * The pi bridge writes the user's row *before* the model is asked - "a send is a fact
   * before it is asked of a model", crates/bridge/src/host.rs - so that a message the
   * user sent survives a crash mid-turn. Reloading that session brings the row back in
   * the page, and painting `sentDraft` on top of it put the same sentence on screen
   * twice. That is the report: "switch from a session that is answering to another and
   * back, and my first message has been duplicated."
   *
   * Only the newest user row can be this turn's - every earlier one already has an
   * answer under it - so the scan stops at the first user row from the end.
   */
  function storeHoldsSentDraft(rows: StoredMessage[]): boolean {
    if (!sentDraft) return false
    const text = sentDraft.text.trim()
    for (let i = rows.length - 1; i >= 0; i--) {
      const m = rows[i]
      if (m.role !== 'user') continue
      return (
        (m.content ?? '').trim() === text &&
        (m.attachments?.length ?? 0) === sentDraft.atts.length
      )
    }
    return false
  }

  // Draft, not a new row: a fresh session is a blank view. The row appears when
  // the first message is sent, already titled by that message.
  function newSession() {
    // The engine's own "start over": a fresh session row arrives on the first send, and
    // `clear_session` releases the host's hold on the previous one so its history is not
    // carried into the new mode/folder.
    try {
      invoke('clear_session')
    } catch (e) {
      pageLog(`clear_session failed: ${e}`)
    }
    showStartScreen()
    void loadSessions()
  }

  /**
   * Back to the screen the app opens on: the cup, the composer, nothing else.
   *
   * This is a *view* reset, not a deletion. The conversation being left keeps its rows in
   * the store, and a turn that is still running keeps writing into its own session - the
   * event router buffers output that does not belong to the view on screen, and opening the
   * session again replays it. Nothing here may touch that.
   */
  function showStartScreen() {
    viewId = null
    // A draft's folder is a choice about *that* draft, so it belongs to the draft and not to
    // the screen. Carrying it across a new session or a mode switch is how the report reads as
    // "the top `+` started me in a folder I never picked": the header's `+` asks for a
    // conversation with no folder chosen, and the chip then says what the host will actually
    // use - "No workspace" when that is the scratch folder, which is the honest name for it.
    draftWorkspace = ''
    if (transcript) transcript.sessionId = 0
    viewMsgCount = 0
    hasOlder = false
    oldestId = null
    transcript?.clear()
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
  /**
   * Forget a conversation, and forget that we were looking at it.
   *
   * The Rust half was already right: `delete_session` refuses while that row's turn is
   * running, and otherwise moves the host off the row before removing it. The screen
   * half was not — the transcript kept painting a conversation that no longer existed,
   * and the composer stayed armed against an id the engine cannot load, so the next
   * message went at a conversation the reader had just deleted. The view goes back to
   * the start screen; the host's next `session_state` says where we actually are.
   */
  async function removeSession(id: number) {
    try {
      await invoke('delete_session', { id })
      if (viewId === id) showStartScreen()
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

  /** What `session_messages` returns when nobody asks for a size. */
  const PAGE_ROWS = 80
  /** Ceiling on a repaint that has to preserve an already-loaded window. */
  const RESYNC_MAX_ROWS = 1200

  async function loadTranscript(id: number, atLeastRows = 0): Promise<StoredMessage[]> {
    viewMsgCount = 0
    const want = Math.max(PAGE_ROWS, atLeastRows)
    if (want > RESYNC_MAX_ROWS)
      pageLog(
        `resync: the view held ${atLeastRows} rows; the repaint keeps ${RESYNC_MAX_ROWS} and leaves the rest one click away`,
      )
    const page = await invoke<{ messages: StoredMessage[]; has_older: boolean }>('session_messages', {
      id,
      limit: Math.min(RESYNC_MAX_ROWS, want),
    })
    pageLog(`history: ${page.messages.length} row(s) for session ${id}`)
    transcript?.loadHistory(page.messages)
    // The note explaining this conversation is not stored in it: a resumed history
    // announces itself once, and a repaint used to drop that announcement.
    transcript?.replayNotes(id)
    // A queued session switch can land before the turn has persisted anything,
    // and reloading then would report an in-flight session as empty and drop
    // the view back to the greeting mid-turn.
    viewMsgCount = turnSessionId === id ? Math.max(page.messages.length, 1) : page.messages.length
    hasOlder = page.has_older
    oldestId = page.messages.length ? page.messages[0].id : null
    // The rows, handed back: a caller repainting a session that is mid-turn has to
    // know whether the store already holds the message that turn started with.
    return page.messages
  }

  async function loadOlder() {
    // The session on screen, not the one the engine happens to be working on.
    // These two are different since browsing stopped moving the host, and asking
    // the host's session for older rows while the user is reading another one
    // returned a page that belonged to a different conversation - or nothing at
    // all, which is what made the button simply vanish: `has_older` for the wrong
    // session was false.
    const target = viewId
    if (target === null || oldestId === null || loadingOlder) return
    loadingOlder = true
    try {
      const page = await invoke<{ messages: any[]; has_older: boolean }>('session_messages', {
        id: target,
        before: oldestId,
      })
      if (page.messages.length) {
        const { shift } = transcript?.prependHistory(page.messages) ?? { shift: 0 }
        // Prepending pushes the viewport down; anchor it back to where the
        // reader was instead of dumping them at the top of the new page.
        const sc = transcript?.scrollEl()
        if (sc) sc.scrollTop += shift
        oldestId = page.messages[0].id
        viewMsgCount += page.messages.length
      } else if (page.has_older) {
        // The store says there is more and returned none: keep the button, say
        // what happened, and do not let `oldestId` walk off the end.
        pageLog(`load_older: session ${target} reported older messages but returned 0 (before id ${oldestId})`)
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
  async function setThinking(l: ReasoningLevel, persist: boolean) {
    // Two dials, one control, two different asks. `persist` is which one was moved: the settings
    // screen sets the effort every *new* conversation starts at, the composer overrides it for
    // *this* conversation alone. Keeping them apart is what makes the composer dial trustworthy -
    // and it is why the override is sent with persist=false: writing the config file from here is
    // how a choice made for one chat used to become everyone's default.
    thinking = l
    if (persist) thinkingDefault = l
    await invoke('set_reasoning', { level: l, persist }).catch((e) => pageLog(`set_reasoning: ${e}`))
  }

  /**
   * Cowork or chat. The host decides, and the tab follows `session_state` rather than
   * what this window last clicked: the switch restarts the model process (tools and the
   * system prompt are chosen when it starts), a second window sees the same truth, and
   * a switch asked for during a turn waits for the turn.
   */
  const personaNow = $derived<'cowork' | 'chat'>(
    hostState?.persona === 'chat' ? 'chat' : hostState?.persona === 'cowork' ? 'cowork' : modeTab,
  )
  async function setPersona(p: 'cowork' | 'chat') {
    if (personaNow === p) return
    modeTab = p // optimistic; refreshState below replaces it with the host's answer
    try {
      const r: any = await invoke('set_persona', { persona: p })
      if (r?.note) pageLog(`persona: ${r.note}`)
    } catch (e) {
      // The rust core refuses this in words. Say so here rather than leaving the tab
      // flipped over a change that never happened.
      pageLog(`set_persona: ${e}`)
    }
    await refreshState()
    // A mode is a new kind of conversation, not the same one with the tools switched off,
    // so the switch lands the user back on the start screen - the cup and the composer -
    // whether it went in or was refused. Two reasons, both about lying:
    //
    //  * the switch restarts the model process (tools and the system prompt are chosen when
    //    it starts), so the transcript still on screen is a record of a conversation the
    //    app can no longer continue in that mode;
    //  * a refused switch leaves the tab honest about the old mode, and the screen should
    //    show the mode the tab claims rather than a half-and-half view of both.
    //
    // Only the view moves. The conversation keeps its rows and its place in the sidebar, and
    // a turn still running keeps writing into its own session (see `showStartScreen`).
    showStartScreen()
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
    /**
     * Which check groups this run was asked for, read from the environment by the app.
     *
     * A full pass costs minutes, and most days the change being checked is one line of CSS or one
     * label. So each named group can be run on its own: `./scripts/selftest.sh cup` runs the cup
     * checks and nothing else, and the filter matches the group id or any word in the section's own
     * label, because "cup" is what you remember and not what you have to look up.
     *
     * What it does *not* do is pretend a partial pass is a pass: the count of groups that ran, and
     * the names of the ones that did not, go into the final line, and the harness says TARGETED at
     * the top of the run. Groups were never designed to depend on each other - each one makes its
     * own session and clears the transcript - but an unnamed block in the middle of a chain is setup
     * for the chain, so those always run.
     */
    let onlyTerms: string[] = []
    const groups = { ran: 0, skipped: [] as string[] }
    try {
      const cfg = await invoke<{ only?: string[] }>('selftest_config')
      onlyTerms = (cfg?.only ?? []).map((t) => String(t).trim().toLowerCase()).filter(Boolean)
    } catch (e) {
      pageLog(`selftest note: could not read the check filter (${e}); running every group`)
    }
    const want = (id: string, label: string) => {
      if (!onlyTerms.length) {
        groups.ran++
        return true
      }
      const hay = `${id} ${label}`.toLowerCase()
      if (onlyTerms.some((t) => hay.includes(t))) {
        groups.ran++
        return true
      }
      groups.skipped.push(id)
      return false
    }
    if (onlyTerms.length)
      pageLog(
        `selftest note: TARGETED run, filter [${onlyTerms.join(', ')}]. A green here covers the groups that ran; ` +
          'the suite is ./scripts/selftest.sh with no arguments',
      )
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
        await frame(0)
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
      // ---- what the pinned row claimed before anyone clicked it --------------
      //
      // The bottom toolbar used to print these facts as words. It is gone, and most of the facts went
      // with it (the search provider lives in Settings now). Two survived, because they are the two
      // that decide whether you need to act: is the gateway up, and is this window in the approval
      // mode the engine is actually in. The gateway fact now rides on the icon's title and a dot, and
      // it is still a claim the app makes *before* anyone opens the panel - which is the whole reason
      // this check exists: an "off" a phone contradicts is a claim about a network-exposed agent.
      {
        const gwBtn = document.querySelector('button[aria-label="Remote access"]') as HTMLElement | null
        if (!gwBtn) fail('the sidebar footer has no gateway control, so nothing states whether the gateway is up')
        else {
          const g = await invoke<{ running: boolean; port: number; note: string }>('gateway_status')
          const dot = gwBtn.querySelector('.sfi-dot') ? 'lit' : 'unlit'
          const title = gwBtn.getAttribute('title') ?? ''
          if (g.running && !title.includes(String(g.port)))
            fail(
              `the gateway control's title is "${title}" while it is serving on :${g.port}: the row is a placeholder that only fills in when opened`,
            )
          else if (!g.running && gwBtn.querySelector('.sfi-dot'))
            fail('the gateway control carries a live dot while the gateway is not running')
          const hostBoot = await invoke<HostState>('session_state')
          const modeChip = [...document.querySelectorAll('.cbtn.chip')]
            .map((c) => (c.textContent ?? '').trim())
            .find((t) => Object.values(MODE_LABEL).some((v) => t.includes(v)))
          if (!modeChip) fail('the composer shows no approval-mode chip')
          else if (!modeChip.includes(MODE_LABEL[hostBoot.mode]))
            fail(
              `the mode chip reads "${modeChip}" while the engine is in mode=${hostBoot.mode}: ` +
                'two clients of one engine are showing two different answers',
            )
          else
            pageLog(
              `pinned row truth ok (gateway title "${title}" with a ${dot} dot for running=${g.running}, mode chip "${modeChip}" for mode=${hostBoot.mode})`,
            )
        }
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
    // rAF is not a clock. An occluded, minimized or workspace-switched window stops
    // compositing, and a wait built only on requestAnimationFrame then never settles:
    // the run prints no verdict and no failure, and the harness's only signal is a
    // fifteen-minute watchdog. Settle on a floor instead and say so once per episode,
    // so a long chain of frame waits costs one honest skip rather than thirty.
    let rafStalls = 0
    let lastStallLogged = 0
    // Paints the compositor actually delivered. Animation progress is only
    // observable while this moves: an occluded window freezes CSS animations at
    // their starting values, and a check that then asks "is this word still
    // half-drawn?" is measuring the window manager.
    let framePaints = 0
    /** What the run has already spent waiting for a compositor that stopped sending frames. */
    let reviveSpentMs = 0
    const REVIVE_TOTAL_MS = 60_000
    /** Wait for `paints` real paints, settling on a floor if they never come.
     *  Resolves true when the page actually painted, false when the floor won. */
    function frame(settleMs = 0, paints = 1): Promise<boolean> {
      return new Promise((res) => {
        let left = paints
        let settled = false
        let timer = 0
        let lastAsk = 0
        const step = () => {
          if (settled) return
          left -= 1
          if (left <= 0) {
            settled = true
            clearTimeout(timer)
            framePaints += 1
            setTimeout(() => res(true), settleMs)
            return
          }
          requestAnimationFrame(step)
        }
        requestAnimationFrame(step)
        const floor = () => {
          if (settled) return
          // A silent frame clock means the desktop stopped compositing this window; it does not mean
          // the page cannot paint. On Wayland an app cannot take focus back - mutter decides - so the
          // honest move is to wait a few seconds, asking once in a while, and only then admit the
          // clock is gone. Writing off 31 checks measures the window manager either way; this at
          // least stops measuring it slowly, and it catches the common case where the window comes
          // back on its own between two checks.
          if (reviveSpentMs < REVIVE_TOTAL_MS) {
            reviveSpentMs += 1500
            const now = performance.now()
            if (now - lastAsk > 4000) {
              lastAsk = now
              void presentWindow('the frame clock went silent')
            }
            timer = setTimeout(floor, 1500)
            return
          }
          settled = true
          clearTimeout(timer)
          rafStalls += 1
          const now = Date.now()
          if (now - lastStallLogged > 5000) {
            lastStallLogged = now
            blindSkips += 1
            pageLog(
              `selftest note: requestAnimationFrame stayed silent for ${400 + settleMs + reviveSpentMs}ms - the page is not ` +
                'being composited, so the frame-paced checks are settling on a timeout floor instead of on paints',
            )
          }
          setTimeout(() => res(false), settleMs)
        }
        timer = setTimeout(floor, 400 + settleMs)
      })
    }
    /** A page can be perfectly laid out and still not be painted: that is what an occluded window
     *  looks like from the inside, and it is invisible to a layout test. Motion claims need paints,
     *  so they ask this instead of `offScreen()`, which only answers the layout question. */
    const unpainted = () =>
      framePaints < 4 || rafStalls > framePaints * 3
        ? `the page reported ${framePaints} paint(s) against ${rafStalls} frame-clock timeout(s)`
        : null
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
      // Second recovery: ask for the window back. Recent blind runs reported the same shape -
      // frame gaps, follow catch-up that never converged, geometry checks reading 0x0 - and the
      // cause was another application covering this one, not the code under test. A window that
      // is not composited cannot answer a single layout question, so it is worth one ask before
      // twelve checks are written off.
      await presentWindow(where)
      b = offScreen()
      if (!b) {
        pageLog(`selftest note: the window came back for ${where}`)
        return false
      }
      blindSkips++
      pageLog(`selftest note: skipped ${where}; ${b}`)
      return true
    }
    await presentWindow('the start')
    if (await blind('at the start')) {
      pageLog('SELFTEST ABORTED: nothing was measured')
      return
    }
    // The compositor owns the frame clock, and this desktop will not hand focus back to an app that
    // asks for it: `set_focus` from a background window is refused as focus stealing, and everything
    // downstream - rAF, CSS animations, sticky geometry - goes quiet. Only a person clicking the
    // window fixes that. So the run says what it needs and waits for it once, up front: waiting a
    // minute produces a measured run, while pushing on produces thirty-odd notes saying nothing was
    // observable and a verdict nobody can act on.
    if (transportKind === 'tauri') {
      const state = async () => {
        try {
          return await invoke<string>('selftest_window_state')
        } catch {
          return ''
        }
      }
      const focusedNow = async () => (await state()).includes('focused=true')
      if (!(await focusedNow())) {
        pageLog(
          'selftest note: the desktop is not compositing this window (window state: ' +
            (await state()) +
            '). Layout, motion and cadence claims cannot be measured until it has focus, and an app cannot take focus back on Wayland - clicking the cwoffee window once does it. Waiting up to 90s.',
        )
        const untilFocus = performance.now() + 90_000
        let beats = 0
        while (performance.now() < untilFocus && !(await focusedNow())) {
          if (beats % 5 === 0) await presentWindow('the run is waiting for the window')
          beats += 1
          await new Promise((r) => setTimeout(r, 1000))
        }
        pageLog(
          (await focusedNow())
            ? 'selftest note: the window has the desktop focus; the layout and paint claims below are measured, not skipped'
            : 'selftest note: the window never got the desktop focus; the layout, motion and cadence claims below are graded blind',
        )
      }
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
        await invoke('set_reasoning', { level: reasoning0, persist: true }).catch(() => {})
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
        await invoke('set_reasoning', { level: 'medium', persist: true })
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
        // The host moves between commands, and on the pi core the command queue can be
        // holding a model round trip, so the row that was clicked and the session the host
        // names can differ for a moment. Wait for the move — bounded, and timed — because a
        // switch that lands late is a different fact from one that never lands, and the
        // message has to be able to tell them apart.
        let h = await invoke<HostState>('session_state')
        let waited = 0
        while (h.session_id !== id && waited < 2000) {
          await new Promise((r) => setTimeout(r, 100))
          waited += 100
          h = await invoke<HostState>('session_state')
        }
        if (h.session_id !== id)
          fail(`useSession(${id}): the host is still on ${h.session_id} after ${waited}ms`)
        if (waited >= 400) pageLog(`switch_session(${id}) took ${waited}ms to land`)
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
        // Notes are not in the transcript any more, so "browsing said nothing about host
        // plumbing" is checked against what is left in the stream plus whatever the chrome is
        // currently holding.
        const noise = [
          ...document.querySelectorAll('.blk-warn'),
          ...(document.querySelectorAll('.toast:not(.note)') as NodeListOf<HTMLElement>),
        ]
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

      // --- the thinking level: a default, and an override that belongs to one conversation ----
      // Reported as: set a default in settings, start a new conversation, move the composer's
      // thinking dial, press send - and the level goes back to the default. It is not a coincidence
      // that send is the moment: a conversation's row (and, on the pi core, its pi thread) is
      // created by the first send, and the level had been applied to a session that the send then
      // replaced. What should happen, in the words of the report: settings is the default, a new
      // conversation starts at it, and a composer choice overrides it for that conversation alone,
      // for the whole conversation, until moved again.
      if (want('thinking', 'the thinking level: a default, and an override that belongs to one conversatio')) {
        const verdict: string[] = []
        const st = async () => invoke<HostState>('session_state')
        const nap = (ms: number) => new Promise((r) => setTimeout(r, ms))
        // A conversation switch during a running turn is deliberately deferred - the turn finishes
        // first, and that is right for a person who pressed New chat mid-answer. It scrambles a
        // check that issues the commands back to back: the switch, the dial move and the send all
        // land in a different order than they were written, and the dial then looks like it was
        // reset. So this check waits for the host to be quiet before moving between conversations,
        // and waits for the move to have actually taken.
        const quiet = async (why: string) => {
          for (let i = 0; i < 80 && (await st()).busy; i++) await nap(100)
          if ((await st()).busy) verdict.push(`the host was still busy for 8s before ${why}`)
        }
        /**
         * Wait for the host to *say* it, rather than assuming the invoke already took.
         *
         * Commands are queued, and a switch that arrives while the previous turn is still unwinding
         * is deliberately applied after it - which is right for a person who pressed New chat
         * mid-answer. Reading state the instant after invoking therefore asks about a command that
         * has not run yet, and a correct pi core starts reporting a dial that was reset. This is the
         * same discipline the engine's own regression test uses: poll for the fields the host
         * publishes together, and say so when they never arrive.
         */
        const until = async (why: string, ok: (s: HostState) => boolean, ms = 3000) => {
          let s = await st()
          for (let i = 0; i < ms / 100 && !ok(s); i++) {
            await nap(100)
            s = await st()
          }
          if (!ok(s))
            verdict.push(
              `the host never published ${why} within ${ms}ms (row ${s.session_id}, reasoning ${s.reasoning}, default ${JSON.stringify(s.reasoning_default)}, overridden ${s.reasoning_overridden})`,
            )
          return s
        }
        // The default, from the settings screen's path.
        await quiet('the settings default')
        await invoke('set_reasoning', { level: 'medium', persist: true })
        let s = await until('the settings default at medium', (x) => x.reasoning_default === 'medium')
        if (s.reasoning_default !== 'medium')
          verdict.push(`the settings screen's own level reads ${JSON.stringify(s.reasoning_default)} after being set to medium`)
        // A new conversation starts at the default, and does not claim an override.
        await quiet('the first new conversation')
        const convA = await invoke<number>('new_session', { title: 'selftest thinking scope', workspace: null })
        // The dial belongs to a conversation, so the host has to be standing in it - and to have
        // said so, because entering a row is where the level for it is applied.
        s = await until(
          `the host on conversation ${convA} at the default with no override of its own`,
          (x) => x.session_id === convA && x.reasoning === 'medium' && x.reasoning_default === 'medium' && !x.reasoning_overridden,
        )
        if (s.session_id !== convA)
          verdict.push(`the host is on conversation ${s.session_id} although the check created ${convA}`)
        if (s.reasoning !== 'medium') verdict.push(`a new conversation starts at ${s.reasoning}, expected the default medium`)
        if (s.reasoning_overridden) verdict.push('a conversation that was never touched reports its own override')
        // The composer dial, moved while this conversation is still a draft. 'off' is chosen on
        // purpose: it is the level that keeps the two turns below cheap.
        await invoke('set_reasoning', { level: 'off', persist: false })
        s = await until(
          "the composer's off on this conversation without moving the default",
          (x) => x.reasoning === 'off' && x.reasoning_default === 'medium' && x.reasoning_overridden === true,
        )
        if (s.reasoning !== 'off') verdict.push(`the composer's choice did not take effect (state says ${s.reasoning})`)
        if (s.reasoning_default !== 'medium')
          verdict.push(`the composer's choice moved the settings default to ${s.reasoning_default}`)
        // Send. This is the step that used to undo the dial, because it is where the conversation
        // starts to exist.
        let p = (await st()).turns
        await invoke('start_turn', {
          text: 'Reply with the single word ok. Use no tools and create no files.',
          attachments: [],
          session: null,
        })
        if (!(await waitTurnDone(p))) verdict.push('the scope check turn never finished')
        s = await st()
        if (s.reasoning !== 'off')
          verdict.push(`sending the message reverted the thinking level to ${s.reasoning} - this is the reported bug`)
        if (!s.reasoning_overridden) verdict.push('after the send the conversation no longer reports its own override')
        if (s.reasoning_default !== 'medium')
          verdict.push(`after the send the settings default reads ${s.reasoning_default}, expected medium`)
        // A second turn in the same conversation keeps it.
        p = (await st()).turns
        await invoke('start_turn', {
          text: 'Reply with the single word ok again. Use no tools and create no files.',
          attachments: [],
          session: null,
        })
        if (!(await waitTurnDone(p))) verdict.push("the scope check's second turn never finished")
        s = await st()
        if (s.reasoning !== 'off') verdict.push(`the second turn in the same conversation ran at ${s.reasoning}, expected off`)
        // And a different conversation is untouched by all of it.
        await quiet("the second new conversation")
        const convB = await invoke<number>('new_session', { title: 'selftest thinking scope two', workspace: null })
        s = await until(
          `the host on conversation ${convB} back at the default`,
          (x) => x.session_id === convB && x.reasoning === 'medium' && x.reasoning_default === 'medium' && !x.reasoning_overridden,
        )
        if (s.session_id !== convB)
          verdict.push(`the host is on conversation ${s.session_id} although the check created ${convB}`)
        if (s.reasoning !== 'medium')
          verdict.push(`a second new conversation inherited ${s.reasoning}; one chat's choice must not travel`)
        if (s.reasoning_overridden) verdict.push("a second conversation claims the first one's override")
        // What this model says it can answer for. On the pi core this is pi's own answer, and it is
        // where the dial used to be wired to nothing: a model entry that declares no thinking offers
        // only "off", and every level the reader picked was clamped away before the request was even
        // built - the dial moved, the wire said nothing, and nothing anywhere reported it. An empty
        // list is a finding rather than a skip: either nobody asked the model, or its entry declares
        // no thinking, and both are worth knowing out loud.
        s = await until('the thinking levels this model offers', (x) => (x.reasoning_levels?.length ?? 0) > 0, 5000)
        const offered = s.reasoning_levels ?? []
        if (!offered.length) {
          verdict.push('the host published no thinking levels at all: nobody asked the model what it can run, or its model entry declares no thinking and pi offers only "off"')
        } else {
          for (const need of ['low', 'medium', 'high']) {
            if (!offered.includes(need))
              verdict.push(`the dial offers ${need} but the model says it cannot answer for it (offered ${JSON.stringify(offered)})`)
          }
          if (!offered.includes(String(s.reasoning)))
            verdict.push(`this conversation is set to ${s.reasoning}, which the model does not offer (offered ${JSON.stringify(offered)})`)
        }
        if (verdict.length) {
          const f = await st()
          fail(
            `thinking scope: ${verdict.join('; ')} [host row ${f.session_id}, reasoning ${f.reasoning}, default ${f.reasoning_default}, overridden ${f.reasoning_overridden}, offered ${JSON.stringify(f.reasoning_levels ?? [])}]`,
          )
        }
        else
          pageLog(
            `thinking scope ok: settings is the default, a new conversation starts there, the composer override survived the send that creates the conversation, kept it through a second turn, and did not move the default; this model says it answers for ${JSON.stringify(offered)}`,
          )
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
        await invoke('set_reasoning', { level: 'off', persist: true })
        await invoke('start_turn', {
          text: 'What colour is this image? Answer in one word.',
          attachments: [{ mime: 'image/png', data_b64: PNG1x1, name: 'probe.png' }],
          session: target.id,
        })
        const done = await waitTurnDone(turnsBefore)
        await invoke('set_reasoning', { level: 'medium', persist: true })
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

      // --- the picture has to reach the endpoint, not merely the store -----------
      // The check above proves the row kept its metadata; it does not prove the model
      // was handed the bytes. A core that dropped the attachment and stored it anyway
      // would pass that and lie. The scripted endpoint now answers with the image
      // blocks it actually received, so the transcript is the receipt, and the same
      // case runs against either core.
      if (want('clipboard', 'the picture has to reach the endpoint, not merely the store')) {
        const attSess = await invoke<number>('new_session', { title: 'selftest attach' })
        await useSession(attSess)
        const turnsBeforeAtt = await turnCount()
        // The same 1x1 PNG the vision check sends. It is declared inside that block;
        // re-declaring the literal here is cheaper than rewiring a passing check, and
        // both are named for what they are: a probe image, not a picture of anything.
        const PNG_PROBE =
          'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGNgYGBgAAAABQABpfZFQAAAAABJRU5ErkJggg=='
        await invoke('start_turn', {
          text: 'ATTACHMENT ECHO',
          attachments: [{ mime: 'image/png', data_b64: PNG_PROBE, name: 'echo.png' }],
          session: attSess,
        })
        const attDone = await waitTurnDone(turnsBeforeAtt)
        if (!attDone) {
          pageLog('selftest note: skipped the endpoint-side attachment check; the turn did not finish')
        } else {
          const st = await invoke<HostState>('session_state')
          const page = await invoke<any>('session_messages', { id: st.session_id })
          const said = (page.messages ?? [])
            .filter((m: any) => m.role === 'assistant')
            .map((m: any) => String(m.content ?? ''))
          const receipt = said.find((t: string) => t.includes('Attachment seen:'))
          if (!receipt)
            fail(`the endpoint never reported an attachment (last answer: ${said.slice(-1)[0] ?? 'nothing'})`)
          else if (!receipt.includes('1 image(s): image/png'))
            fail(`the endpoint saw the wrong thing: "${receipt}"`)
          const kept = (page.messages ?? [])
            .filter((m: any) => Array.isArray(m.attachments) && m.attachments.length)
            .flatMap((m: any) => m.attachments)
            .find((x: any) => x.name === 'echo.png')
          if (!kept) fail('the attachment name the client sent did not survive into the store')
          else if (receipt?.includes('1 image(s): image/png'))
            pageLog(`attachment reaches the model (${receipt})`)
        }
        await invoke('delete_session', { id: attSess }).catch(() => {})
      }

      // Memory, in the app's own board. Two claims live here: the card reads "Recalled"
      // rather than a generic "Done", and the *other* conversation is what comes back, with
      // the asking one named and excluded. The second one is the interesting failure — on the
      // pi core the tool host runs three processes away from the store, so if the database
      // path or the session map never arrives the tool answers "this host cannot say which
      // conversation is asking". That is an honest sentence, and it is still a broken feature,
      // which is why the check fails on it instead of accepting it.
      {
        const memA = await invoke<number>('new_session', { title: 'board memory alpha' })
        const memB = await invoke<number>('new_session', { title: 'board memory beta' })
        let mBefore = await turnCount()
        await invoke('start_turn', {
          text: 'SEED THE MEMORY: remember the word mangosteen for later.',
          session: memA,
        }).catch((e) => pageLog(`selftest note: the seed turn failed: ${e}`))
        const seeded = await waitTurnDone(mBefore, 60000)
        mBefore = await turnCount()
        await invoke('start_turn', {
          text:
            'RECALL GATE: look up what I have said about mangosteen in another conversation, then say RECALL DONE.',
          session: memB,
        }).catch((e) => pageLog(`selftest note: the recall turn failed: ${e}`))
        const askedBack = await waitTurnDone(mBefore, 60000)
        await useSession(memB)
        await new Promise((r) => setTimeout(r, 400))
        const memCard = [...document.querySelectorAll('.blk-tool[data-tool="recall"]')].pop()
        const memHead = memCard?.querySelector('.tool-name')?.textContent ?? ''
        const memOut = memCard?.querySelector('.tool-out')?.textContent ?? ''
        if (!seeded || !askedBack) fail('one of the two memory turns never finished')
        else if (!memCard) fail('no tool card for recall: the model never reached the memory tool')
        else if (!memHead.includes('Recalled'))
          fail(`the recall card does not read "Recalled": ${JSON.stringify(memHead.slice(0, 40))}`)
        else if (!memOut.includes('board memory alpha'))
          fail(`the recall card does not name the other conversation: ${JSON.stringify(memOut.slice(0, 140))}`)
        else if (memOut.includes('cannot say which conversation is asking'))
          fail('the tool host could not tell which conversation was asking: the store path or the session map never reached it')
        else if (memOut.includes('board memory beta'))
          fail('the recall card quoted the asking conversation back to itself')
        else
          pageLog(
            `recall ok (card reads ${JSON.stringify(memHead)}, names the other conversation, excludes this one)`,
          )
        for (const id of [memA, memB]) await invoke('delete_session', { id }).catch(() => {})
      }

      // The settings screen has to answer two questions at once: is every setting
      // still reachable now that the rail names five categories instead of eleven
      // sections, and does the whole screen share one set of measurements.
      //
      // The first is checked by walking the old names — the names links and this
      // board already use — and insisting each one lands on a page that really holds
      // the section it claims. A rail that hides a section and a rail that dropped it
      // look identical from a screenshot, so the only honest check is to go looking
      // for every one of them. The second is measured, not admired: the heights of
      // the fields, the select trigger against the text inputs, and the colours the
      // open dropdown paints itself with.
      if (want('settings', 'the settings board keeps the design system: names, heights, colours, focus ring, dial')) {
        openSettings('tools')
        await new Promise((r) => setTimeout(r, 900))

        // 1. The rail: category names, no captions.
        const nav = [...document.querySelectorAll('.set-rail [data-set-tab]')] as HTMLElement[]
        const railText = nav.map((b) => (b.textContent ?? '').trim().replace(/\s+/g, ' '))
        if (nav.length !== SET_PAGES.length)
          fail(`the settings rail shows ${nav.length} of ${SET_PAGES.length} categories`)
        else if (document.querySelector('.set-rail .set-hint'))
          fail('the settings rail still carries descriptive subtitles')
        else if (nav.some((t) => (t.textContent ?? '').split(/\s+/).length > 3))
          fail(`a rail entry is a sentence rather than a name: ${JSON.stringify(railText)}`)
        else if (!nav.every((t) => t.querySelector('svg')))
          fail('a rail entry has no icon: five identical words are harder to scan than five marks')
        else pageLog(`settings rail ok (${railText.join(', ')})`)

        // 2. Nothing orphaned: every name that used to be a page must still land.
        const missing: string[] = []
        const landed: string[] = []
        for (const name of Object.keys(SECTION_HOME)) {
          const [wantPage, wantSection] = SECTION_HOME[name]
          openSettings(name)
          await new Promise((r) => setTimeout(r, 90))
          const body = document.querySelector('.set-body')
          const page = body?.getAttribute('data-set-page')
          const sect = body?.querySelector(`[data-set-section="${wantSection}"]`)
          if (page !== wantPage) missing.push(`${name} -> page ${page} (wanted ${wantPage})`)
          else if (!sect) missing.push(`${name} -> no [data-set-section="${wantSection}"] on ${page}`)
          else landed.push(`${name}:${page}/${wantSection}`)
        }
        if (missing.length) fail(`settings destinations lost: ${missing.join('; ')}`)

        // 3. Every page has a heading and a description, and every section has a
        //    heading: the copy moved out of the rail, so it has to be somewhere.
        const pageGaps: string[] = []
        for (const p of SET_PAGES) {
          openSettings(p.kind)
          await new Promise((r) => setTimeout(r, 140))
          const body = document.querySelector('.set-body')
          const t = (body?.querySelector('.spage-title')?.textContent ?? '').trim()
          const d = (body?.querySelector('.spage-desc')?.textContent ?? '').trim()
          const sects = [...(body?.querySelectorAll('[data-set-section]') ?? [])]
          const noHead = sects.filter(
            (el) => !(el.querySelector('.ssect-h')?.textContent ?? '').trim(),
          ).length
          const noDesc = sects.filter(
            (el) => !(el.querySelector('.ssect-d')?.textContent ?? '').trim(),
          ).length
          if (t !== p.title) pageGaps.push(`${p.kind}: title "${t}"`)
          else if (d.length < 40) pageGaps.push(`${p.kind}: description is ${d.length} characters`)
          else if (sects.length < 1) pageGaps.push(`${p.kind}: no sections`)
          else if (noHead || noDesc)
            pageGaps.push(`${p.kind}: ${noHead} section(s) without a heading, ${noDesc} without a description`)
        }
        if (pageGaps.length) fail(`settings page headers: ${pageGaps.join('; ')}`)
        else
          pageLog(
            `settings pages ok (${SET_PAGES.length} titled pages, ${landed.length} names still land, every section headed and described)`,
          )

        // 4. One design system, measured. The complaint this refactor answers is
        //    that the fields were three sizes and the dropdown was painted white by
        //    the toolkit, so the assertions are about size and colour, not taste.
        openSettings('models')
        await new Promise((r) => setTimeout(r, 600))
        const inputs = [...document.querySelectorAll('.set-body .sinp')] as HTMLElement[]
        const sels = [...document.querySelectorAll('.set-body .ssel-btn')] as HTMLElement[]
        const btns = [...document.querySelectorAll('.set-body .set-btn')] as HTMLElement[]
        const hs = (els: HTMLElement[]) => [...new Set(els.map((e) => Math.round(e.getBoundingClientRect().height)))]
        if (inputs.length < 4) fail(`the Models page has ${inputs.length} text fields; the endpoint form should have more`)
        else if (sels.length < 1) fail('the Models page has no themed select: the native <select> is still here')
        else if (hs(inputs).length > 1) fail(`text fields come in ${hs(inputs).length} heights: ${hs(inputs).join('/')}`)
        else if (inputs[0].getBoundingClientRect().height < 29)
          fail(`a settings text field is ${inputs[0].getBoundingClientRect().height}px tall; that is not a desktop control`)
        else if (Math.abs(sels[0].getBoundingClientRect().height - inputs[0].getBoundingClientRect().height) > 1)
          fail(
            `the select is ${sels[0].getBoundingClientRect().height}px against a ${inputs[0].getBoundingClientRect().height}px field: two controls, one row, two sizes`,
          )
        else if (hs(btns).length > 1) fail(`buttons come in ${hs(btns).length} heights: ${hs(btns).join('/')}`)
        else {
          // The open dropdown has to be the app's colour, not the toolkit's.
          sels[0].click()
          await new Promise((r) => setTimeout(r, 120))
          const pop = document.querySelector('.ssel-pop') as HTMLElement | null
          const opts = [...document.querySelectorAll('.ssel-opt')] as HTMLElement[]
          if (!pop) fail('pressing the select opened no panel: the themed dropdown is not wired up')
          else if (!opts.length) fail('the select panel is empty')
          else {
            const cs = getComputedStyle(pop)
            const screen = getComputedStyle(document.querySelector('.settings') as Element).backgroundColor
            const ink = getComputedStyle(opts[0]).color
            // Snapshot the colours. `cs` is live: read after the panel has gone, it
            // reports nothing, and a verdict line that quotes "nothing" while the
            // judgement used the real values is a claim nobody can check afterwards.
            const panelBg = cs.backgroundColor
            const panelShadow = cs.boxShadow
            // Colour arithmetic, in the theme the app happens to be in. The complaint
            // this refactor answers was a bright white toolkit menu on a dark screen,
            // and the answer is not "assert the panel is dark" - a light theme is
            // allowed to have a light menu. The claim is that the panel separates
            // from the surface behind it and that its text is readable on it, which
            // is a number either theme can be held to.
            // A colour this code cannot read is a colour it cannot judge, so the
            // parse refuses rather than producing NaN: a comparison against NaN is
            // false, and a check built on false silently becomes a check that
            // nothing can fail.
            const rgb = (what: string, c: string): number[] => {
              const n = (c ?? '').match(/[\d.]+/g)?.map(Number) ?? []
              if (n.length < 3)
                throw new Error(`the page reports no ${what} colour (got ${JSON.stringify(c)})`)
              return n
            }
            const lum = (what: string, c: string) => {
              const [r, g, b] = rgb(what, c)
              const f = (v: number) => {
                const x = v / 255
                return x <= 0.03928 ? x / 12.92 : ((x + 0.055) / 1.055) ** 2.4
              }
              return 0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b)
            }
            const ratio = (a: string, what: string, b: string) => {
              const la = lum('the option text', a)
              const lb = lum(what, b)
              return (Math.max(la, lb) + 0.05) / (Math.min(la, lb) + 0.05)
            }
            const dist = (a: string, b: string, what: string) => {
              const x = rgb(what, a)
              const y = rgb('the surface behind it', b)
              return Math.max(Math.abs(x[0] - y[0]), Math.abs(x[1] - y[1]), Math.abs(x[2] - y[2]))
            }
            const body = document.querySelector('.set-body') as HTMLElement | null
            const pr = pop.getBoundingClientRect()
            const br = body?.getBoundingClientRect()
            const bad: string[] = []
            let lift = NaN
            let contrast = NaN
            try {
              lift = dist(cs.backgroundColor, screen, 'panel')
              contrast = ratio(ink, 'panel', panelBg)
            } catch (e) {
              // Reading the failure out is the point: a colour the page will not
              // name is a claim this check cannot make, and it must not be allowed
              // to pass by producing NaN.
              bad.push(
                `${e instanceof Error ? e.message : e}; panel=${JSON.stringify(panelBg)} screen=${JSON.stringify(screen)} text=${JSON.stringify(ink)}`,
              )
            }
            if (Number.isFinite(lift) && lift < 6)
              bad.push(`panel ${panelBg} on screen ${screen}: no lift, the menu is the page`)
            if (Number.isFinite(contrast) && contrast < 4.5)
              bad.push(`option text ${ink} on ${panelBg} is ${contrast.toFixed(2)}:1`)
            if (panelShadow === 'none') bad.push('the floating panel casts no shadow')
            // The panel is placed inside the scrolling column on purpose (a fixed
            // descendant of a transformed dialog is clipped by it), so the honest
            // check is that it fits the column rather than overflowing it.
            if (br && (pr.top < br.top - 1 || pr.bottom > br.bottom + 1 || pr.left < br.left - 1 || pr.right > br.right + 1))
              bad.push(
                `panel ${pr.top.toFixed(0)}..${pr.bottom.toFixed(0)} escapes the settings column ${br.top.toFixed(0)}..${br.bottom.toFixed(0)}`,
              )
            if (bad.length) fail(`the themed dropdown fails: ${bad.join('; ')}`)
            else {
              sels[0].click()
              await new Promise((r) => setTimeout(r, 80))
              // Focus has to be visible without a mouse: a keyboard user has to see
              // where they are in a form, and a 1px hairline is not that.
              const navBtn = document.querySelector('.set-rail [data-set-tab]') as HTMLElement
              navBtn.focus()
              let ring = getComputedStyle(navBtn).boxShadow
              // A page whose document does not have the desktop's focus paints no `:focus` ring at all:
              // WebKit reports `matches(':focus')` false there, whatever the stylesheet says. That is a
              // fact about the window, not about the app's CSS, so the window is asked for and the ring
              // measured again before anything is judged.
              if (!ring || ring === 'none') {
                await presentWindow('the settings focus ring')
                for (let i = 0; i < 12 && !document.hasFocus(); i++)
                  await new Promise((r) => setTimeout(r, 100))
                navBtn.focus()
                ring = getComputedStyle(navBtn).boxShadow
              }
              if (!ring || ring === 'none' ) {
                if (!document.hasFocus()) {
                  blindSkips++
                  pageLog(
                    'selftest note: skipped the focus-ring claim; this window never got the desktop focus back '
                      + `(activeElement is the button: ${document.activeElement === navBtn}, matches :focus: ${navBtn.matches(':focus')}) - ` +
                      'a document without focus paints no focus ring in any stylesheet',
                  )
                } else {
                  // Four different faults end in the same sentence, so the sentence carries which one:
                  // focus never landed, `:focus` did not match the element that has focus, the ring
                  // custom property is empty here, or the whole document is unfocused.
                  const ncs = getComputedStyle(navBtn)
                  fail(
                    'the focused settings category has no focus ring: the keyboard user cannot see where they are '
                      + `[active is the button: ${document.activeElement === navBtn}, matches :focus: ${navBtn.matches(':focus')}, `
                      + `class "${navBtn.className}", --ring "${ncs.getPropertyValue('--ring').trim() || 'empty'}", `
                      + `document.hasFocus ${document.hasFocus()}, disabled ${(navBtn as HTMLButtonElement).disabled}]`,
                  )
                }
              } else
                pageLog(
                  `settings design system ok (fields ${hs(inputs).join('/')}px, select ${Math.round(sels[0].getBoundingClientRect().height)}px, buttons ${hs(btns).join('/')}px across ${btns.length}; panel ${JSON.stringify(panelBg)} lifts ${lift.toFixed(0)} off ${JSON.stringify(screen)}, option text ${contrast.toFixed(1)}:1, inside the column; focus ring painted)`,
                )
            }
          }
        }

        // 5. The tool list still comes from the engine, not from this file.
        openSettings('tools')
        await new Promise((r) => setTimeout(r, 700))
        const rows = Array.from(document.querySelectorAll('.toolrow .toolname')).map((e) => e.textContent?.trim())
        if (!rows.length) fail('the Tools section lists nothing (tools_view returned no rows)')
        else if (!rows.includes('read_file') || !rows.includes('sheet_query'))
          fail(`the Tools section is not the engine's list: ${JSON.stringify(rows)}`)
        else pageLog(`settings tools section ok (${rows.length} tools from the registry)`)

        // A dial is not a ruler. The reasoning control is five one-word choices; a column stretches
        // its children, which used to turn it into a track the width of the panel with the choices
        // parked at one end of nothing. It has to hug its labels and sit where the panel keeps the
        // things you act on.
        openSettings('reasoning')
        await new Promise((r) => setTimeout(r, 700))
        const dial = document.querySelector('.ssect-b .sseg') as HTMLElement | null
        if (!dial) fail('the reasoning section has no segmented control to measure')
        else {
          const bs = [...dial.querySelectorAll('.sseg-b')].map((b) => b.getBoundingClientRect().width)
          const labels = bs.reduce((a, b) => a + b, 0)
          const dr = dial.getBoundingClientRect()
          if (labels === 0) fail('the reasoning dial has no choices to measure')
          else if (dr.width > labels + 14)
            fail(
              `the dial's track is ${Math.round(dr.width)}px around ${Math.round(labels)}px of choices: it spans the panel instead of hugging them`,
            )
          const panel = dial.closest('.ssect-b') as HTMLElement | null
          if (!panel) fail('the reasoning dial is not inside a section panel')
          else {
            const pr = panel.getBoundingClientRect()
            const padR = parseFloat(getComputedStyle(panel).paddingRight) || 0
            const off = Math.round(pr.right - padR - dr.right)
            if (Math.abs(off) > 5)
              fail(`the dial sits ${off}px off the panel's right inner edge, where this screen keeps its controls`)
            else pageLog(`settings dial ok (${bs.length} choices, ${Math.round(labels)}px of label in a ${Math.round(dr.width)}px track, flush right)`) 
          }
        }

        // The Titles section used to be a cost sheet for a borrowed model: which weights,
        // where the helper script is, how many MB it spikes per conversation. That feature
        // is gone, so the screen now explains the rule that replaced it - and what it checks
        // is that it explains the *trade* (a stalled first answer) and the fallback, while no
        // trace of the old machinery is left to send someone looking for a file that no
        // longer exists.
        {
          openSettings('naming')
          await new Promise((r) => setTimeout(r, 700))
          const txt = document.querySelector('.set-body')?.textContent ?? ''
          const ghosts = ['titlegen', 'helper script', 'weights', 'python', '.py', '870 MB', '861', 'LFM2.5'].filter(
            (w) => txt.toLowerCase().includes(w.toLowerCase()),
          )
          if (!/New Session/.test(txt)) {
            fail(`the Titles section never says what a new conversation is called: ${txt.slice(0, 160)}`)
          } else if (!/Title this conversation in under/.test(txt)) {
            fail('the Titles section does not show the ask the model is given')
          } else if (!/falls back/i.test(txt)) {
            fail('the Titles section hides what happens when the endpoint gives no usable name')
          } else if (ghosts.length) {
            fail(`the Titles section still describes the removed titler: ${ghosts.join(', ')}`)
          } else {
            pageLog('settings titles section ok (placeholder, the ask and the fallback are stated; no trace of the old helper)')
          }
        }
        closeSettings()
      }

      // The endpoint screen, driven the way a person drives it. The verbs behind it have
      // their own Rust tests; what was never proven is that the page round trip works:
      // discovery fills the chips, Add writes a row the list then shows, a window typed into
      // a field outranks what the endpoint claims all the way into the host's stored picture,
      // emptying the field gives that answer back, and what a probe found survives a save that
      // never mentioned it. It ends by removing the row it created, so the board leaves its
      // scratch config as it found it.
      //
      // One straight-line function with early exits rather than a chain of `else` blocks: the
      // steps each need a wait of their own, and the staircase version buried the premises.
      const modelsRoundTrip = async () => {
        openSettings('model')
        await new Promise((r) => setTimeout(r, 500))
        const nameBox = document.querySelector('#cw-pv-name') as HTMLInputElement | null
        const urlBox = document.querySelector('#cw-pv-url') as HTMLInputElement | null
        if (!nameBox || !urlBox) {
          fail('the Models page has no endpoint name/base URL fields: the form is not where the page says it is')
          return
        }
        if (!models.length) {
          fail('the Models page lists no models, so the endpoint round trip could not be driven at all')
          return
        }
        // The case has to point the form at a real endpoint. The address comes from the endpoint
        // picture — `chosen_base_url`, which both cores answer from the config they started with
        // — and not from a model row's `server` field, which names the serving implementation
        // instead ("cwoffee-mock 1.0") and is empty on a core that cannot know it.
        const st0 = await invoke<{ chosen_base_url?: string }>('provider_status')
        const ep = (st0.chosen_base_url ?? '').trim()
        if (!ep) {
          // Say so instead of passing: with no address from the host there is nothing honest for
          // the form to name, and weakening the premise would grade the test, not the app.
          pageLog('models page round trip SKIPPED: the host named no endpoint address (not passed)')
          return
        }
        const type = (el: HTMLInputElement, v: string) => {
          el.value = v
          el.dispatchEvent(new Event('input', { bubbles: true }))
        }
        const act = (txt: string, where: ParentNode = document) =>
          [...where.querySelectorAll('.set-btn')].find((b) => (b.textContent ?? '').includes(txt)) as
            | HTMLButtonElement
            | undefined
        // Half of this case asks the *host*, which is a round trip, so the wait loop is here:
        // bounded, and it answers whether the thing happened instead of pretending.
        const until = async (fn: () => Promise<boolean> | boolean, ms: number) => {
          const t0 = Date.now()
          for (;;) {
            if (await fn()) return true
            if (Date.now() - t0 > ms) return false
            await new Promise((r) => setTimeout(r, 150))
          }
        }
        const before = await invoke<{ providers: { name: string }[] }>('provider_status')
        const rowsBefore = Array.isArray(before?.providers) ? before.providers.length : -1

        const addFirst = act('Add endpoint')
        if (!addFirst) {
          fail('no Add endpoint button on the Models page')
          return
        }
        if (!addFirst.disabled) {
          fail('Add endpoint is live before any discovery: a guess could become configuration, which is what the guard is for')
          return
        }
        type(nameBox, 'selftest probe box')
        type(urlBox, ep)
        // A dispatched `input` updates the state now and the DOM on the next flush: read a
        // control's `disabled` in between and a reachable control reports itself unreachable.
        await renderTick()
        const disc = act('Discover models')
        if (!disc || disc.disabled) {
          fail(
            `Discover models is not reachable once a URL is typed ` +
              `(urlBox.value=${JSON.stringify(urlBox.value)} found=${!!disc} disabled=${disc?.disabled} ` +
              `pvUrl=${JSON.stringify(pvUrl)} probing=${probing} busy=${pvBusy})`,
          )
          return
        }
        disc.click()
        if (!(await until(() => !!document.querySelector('[data-probed] .srow-t'), 12000))) {
          const msg = document.querySelector('[data-pv-msg]')?.textContent ?? 'no message'
          fail(`discovery named nothing for ${ep}: ${msg.slice(0, 140)}`)
          return
        }
        const found = (document.querySelector('[data-probed] .srow-t')?.textContent ?? '').trim()
        const chipNames = [...(document.querySelectorAll('[data-probed] .cap') ?? [])].map((c) => c.textContent?.trim())
        // What discovery claimed about tool calls, read before the add clears the list. Kept to
        // compare after Save: the screen has no field for capabilities, so it has no authority
        // to clear them.
        const toolsYes = probed.filter((c) => c.supports_tools === true).map((c) => c.id)
        if (!chipNames.length) {
          fail(`discovery listed "${found}" with no capability chips: the empty/known/unknown distinction is lost`)
          return
        }
        const test = act('Test connection')
        if (!test) {
          fail('no Test connection button on the Models page')
          return
        }
        test.click()
        // The placeholder message is on screen the moment the click lands, so waiting for "a
        // message" proves nothing — that is how this case once judged a test that had not
        // finished. Wait for the state the button itself reads, then read the sentence.
        if (!(await until(() => !pvBusy, 10000))) {
          fail(`the connection test never finished for ${ep}`)
          return
        }
        const said = (document.querySelector('[data-pv-msg]')?.textContent ?? '').trim()
        if (said.length < 12 || said.startsWith('asking the endpoint')) {
          fail(`the connection test answered with "${said}": that is not a sentence about an endpoint`)
          return
        }
        const add = act('Add endpoint')
        if (!add || add.disabled) {
          fail(`Add endpoint still disabled after discovery found ${found}`)
          return
        }
        add.click()
        if (!(await until(() => !!document.querySelector('[data-set-section="endpoints-list"] .sitem-name'), 6000))) {
          fail('Add endpoint reported success but the page lists no endpoint row')
          return
        }
        const after = await invoke<{ providers: { name: string; models: string[]; base_url: string }[] }>('provider_status')
        const row = (after.providers ?? []).find((x) => x.name === 'selftest probe box')
        if (!row) {
          fail(
            `the host was not told about the endpoint the page added (rows: ${(after.providers ?? [])
              .map((x) => x.name)
              .join(', ')})`,
          )
          return
        }
        if (!row.base_url.includes(ep.replace(/^https?:\/\//, ''))) {
          fail(`the stored row points at ${row.base_url}, not ${ep}`)
          return
        }
        if (!row.models.length) {
          fail('the stored endpoint row came back with no models')
          return
        }
        const mid = row.models[0]
        const prov = [
          ...document.querySelectorAll('[data-set-section="endpoints-list"] .sitem-h'),
        ].find((b) => (b.textContent ?? '').includes('selftest probe box')) as HTMLButtonElement | undefined
        if (!prov) {
          fail('the new endpoint row has no disclosure control')
          return
        }
        // Adding an endpoint opens its row on purpose, so the click is only for the case where
        // it came back shut: clicking an open row would close the thing the case needs.
        if (prov.getAttribute('aria-expanded') !== 'true') {
          prov.click()
          await new Promise((r) => setTimeout(r, 250))
        }
        // Scope to this provider's own block: the list can hold other endpoints, and reaching
        // for the first model row in the section would grade somebody else's model.
        const block = prov.closest('.sitem') as HTMLElement | null
        // The provider row has a `.sitem-top` of its own; the model rows live inside its body,
        // so the model header is looked for there and not at the top of the block.
        const mHdr = block?.querySelector('.sitem-b .sitem-top .sitem-h') as HTMLButtonElement | null
        if (!mHdr) {
          fail('the endpoint row expanded but lists no model row')
          return
        }
        mHdr.click()
        await new Promise((r) => setTimeout(r, 250))
        const winBox = block?.querySelector('input[id^="cw-mw-"]') as HTMLInputElement | null
        if (!winBox) {
          fail('a model row opened with no Max context field')
          return
        }
        // `context_window` on the host's view is what the file holds, not what the endpoint
        // claims: a missing field really does mean nothing is stored.
        const storedWindow = async () => {
          const st = await invoke<{ models?: { id: string; context_window?: number | null }[] }>('provider_status')
          return (st.models ?? []).find((m) => m.id === mid)?.context_window ?? null
        }
        type(winBox, '40000')
        await renderTick()
        const save = act('Save', block ?? document)
        if (!save) {
          fail('the model row has no Save button')
          return
        }
        if (save.disabled) {
          fail('Save is disabled with a number typed in the context field')
          return
        }
        save.click()
        if (!(await until(async () => (await storedWindow()) === 40000, 4000))) {
          fail(`the stored window came back as ${await storedWindow()}, not 40000`)
          return
        }
        const chipsIn = () => [...(block?.querySelectorAll('.cap') ?? [])].map((c) => c.textContent ?? '')
        const wantChip = `${fmtTokens(40000)} context`
        if (!(await until(() => chipsIn().some((c) => c.includes(wantChip)), 2000))) {
          fail(`the model row does not show the stored window it just saved (chips: ${chipsIn().join(', ')})`)
          return
        }
        if (toolsYes.includes(mid)) {
          const kept = (
            await invoke<{ models?: { id: string; supports_tools?: boolean | null }[] }>('provider_status')
          ).models?.find((m) => m.id === mid)
          if (kept?.supports_tools !== true) {
            fail(
              `the save dropped what discovery had found for ${mid}: supports_tools=${String(
                kept?.supports_tools,
              )}, and discovery had said true`,
            )
            return
          }
        }
        // An empty field means "ask the endpoint", so saving it empty has to remove the value
        // rather than file a placeholder — and the page has to stop claiming the number.
        type(winBox, '')
        await renderTick()
        const clear = act('Save', block ?? document)
        if (!clear) {
          fail('the model row lost its Save button')
          return
        }
        clear.click()
        if (!(await until(async () => (await storedWindow()) === null, 4000))) {
          fail(`clearing the field left a stored value behind: ${await storedWindow()}`)
          return
        }
        if (!(await until(() => !chipsIn().some((c) => c.includes(wantChip)), 2000))) {
          fail(`the host no longer stores a window for ${mid} but the row still shows "${wantChip}"`)
          return
        }
        // Leave the scratch config as the run found it.
        const rm = [...(block?.querySelectorAll('.set-btn.danger') ?? [])].find((b) =>
          (b.textContent ?? '').includes('Remove endpoint row'),
        ) as HTMLButtonElement | undefined
        if (!rm) {
          fail('the endpoint row offers no way to remove it')
          return
        }
        rm.click()
        if (
          !(await until(async () => {
            const st2 = await invoke<{ providers: { name: string }[] }>('provider_status')
            return !(st2.providers ?? []).some((x) => x.name === 'selftest probe box')
          }, 5000))
        ) {
          fail('the board could not remove the endpoint row it created')
          return
        }
        const now = await invoke<{ providers: unknown[] }>('provider_status')
        pageLog(
          `models page ok (discovered ${found}, tested, added a row the host kept, stored 40000 then ` +
            `freed it, what the probe found survived a save that never mentioned it, removed the row: ` +
            `${rowsBefore} -> ${(now.providers ?? []).length})`,
        )
      }
      await modelsRoundTrip()
      closeSettings()

      // Cowork <-> chat, driven through the tab the user actually clicks. The scripted
      // endpoint answers the same prompt with a tool call on both halves: in chat nothing
      // may run and no card may open; in cowork the card must come back. A toggle that
      // only repaints a button passes a click test and fails this one.
      //
      // Two lessons this check enforces because it got them wrong first. It needs a
      // baseline: an earlier block can leave the host in Auto, where no card opens in
      // EITHER mode, and then the halves look identical and the check is worth nothing —
      // so it puts the host in ask mode for its own duration. And it must judge only the
      // rows the chat turn added: the cowork half writes its own tool rows into the same
      // conversation, and reading the whole transcript blamed those instead.
      {
        const modeSess = await invoke<number>('new_session', { title: 'selftest mode' })
        await useSession(modeSess)
        const mode0 = (await invoke<HostState>('session_state')).mode
        await invoke('set_mode', { mode: 'ask' }).catch(() => {})
        const TRIG = 'Use read_file to read the file /etc/hostname'
        const tabs = Array.from(document.querySelectorAll<HTMLButtonElement>('.modesel'))
        const chatTab = tabs.find((b) => b.textContent?.trim() === 'Chat')
        const workTab = tabs.find((b) => b.textContent?.trim() === 'Cowork')
        if (!chatTab || !workTab) {
          fail('the Cowork/Chat tabs are missing from the composer')
        } else {
          const personaOf = async () => (await invoke<HostState>('session_state')).persona
          const cardUp = async (ms: number) => {
            const until = Date.now() + ms
            while (Date.now() < until) {
              if (document.querySelector('.approval')) return true
              await new Promise((r) => setTimeout(r, 60))
            }
            return false
          }
          const dismissCard = () =>
            document.querySelector<HTMLButtonElement>('.approval .btn-bad')?.click()
          const rowsNow = async () =>
            (((await invoke<any>('session_messages', { id: modeSess })).messages ?? []) as any[])
          let ok = true
          if ((await personaOf()) !== 'cowork') {
            fail(`a cowork host reports ${(await personaOf())}`)
            ok = false
          }
          let before = await turnCount()
          await invoke('start_turn', { text: TRIG, session: modeSess })
          if (!(await cardUp(6000))) {
            fail('cowork in ask mode raised no approval card for a tool call — the comparison has no baseline')
            ok = false
            await waitTurnDone(before).catch(() => {})
          } else {
            dismissCard()
            if (!(await waitTurnDone(before))) {
              fail('the cowork turn never finished after Deny')
              ok = false
            }
          }
          if (ok) {
            chatTab.click()
            await new Promise((r) => setTimeout(r, 500))
            const after = await personaOf()
            if (after !== 'chat' && after !== 'cowork') fail(`the host reports ${after} after a mode switch`)
            else if (after === 'cowork')
              // The rust core refuses this switch in words. What must hold either way is
              // that the tab does not claim a mode the host refused to enter.
              pageLog('selftest note: this core refused chat mode and the tab stayed honest (cowork)')
            else {
              before = await turnCount()
              const rowMark = (await rowsNow()).length
              await invoke('start_turn', { text: TRIG, session: modeSess })
              if (await cardUp(3000)) {
                dismissCard()
                fail('chat mode opened an approval card — a chat with tools is still the coworker')
              } else if (!(await waitTurnDone(before))) fail('the chat turn never finished')
              else {
                const added = (await rowsNow()).slice(rowMark)
                // pi answers a call for a tool it does not have with its own "not found"
                // result; anything else means a cwoffee tool ran inside a chat.
                const ranTool = added
                  .filter((m: any) => m.role === 'tool')
                  .filter((m: any) => !/not found/i.test(String(m.content ?? '')))
                if (ranTool.length)
                  fail(
                    `a tool ran inside chat mode: ${JSON.stringify(ranTool.map((m: any) => String(m.content).slice(0, 40)))}`,
                  )
                else {
                  workTab.click()
                  await new Promise((r) => setTimeout(r, 500))
                  if ((await personaOf()) !== 'cowork')
                    fail(`clicking Cowork left the host at ${await personaOf()}`)
                  else
                    pageLog('mode toggle ok (chat runs no tools and opens no card, cowork opens the card again)')
                }
              }
            }
          }
          await invoke('set_mode', { mode: mode0 }).catch(() => {})
          await invoke('delete_session', { id: modeSess }).catch(() => {})
        }
      }

      // Chat is a different list, not a narrower folder tree. A chat owns no folder, so
      // filing one under a workspace makes the sidebar claim something about it that is
      // not true - and the only way to reach it would be to guess which bucket the boot
      // directory landed in.
      {
        const chatSess = await invoke<number>('new_session', { title: 'selftest chat list' })
        // Same tabs the mode block drives: the tab is the only way in, so the check goes
        // through it rather than poking the state behind the app's back.
        const tabs = Array.from(document.querySelectorAll<HTMLButtonElement>('.modesel'))
        const chatTab = tabs.find((b) => b.textContent?.trim() === 'Chat')
        const workTab = tabs.find((b) => b.textContent?.trim() === 'Cowork')
        const personaOf = async () => (await invoke<HostState>('session_state')).persona
        if (!chatTab || !workTab) {
          fail('the Cowork/Chat tabs are missing, so the chat list cannot be reached')
          await invoke('delete_session', { id: chatSess }).catch(() => {})
        } else
        try {
          await invoke('set_session_persona', { id: chatSess, persona: 'chat' })
          const rows = await invoke<Session[]>('list_sessions')
          sessions = rows
          const mine = rows.find((r) => r.id === chatSess)
          if (!mine) fail('the store has no row for the chat it just created')
          else if (mine.persona !== 'chat')
            fail(`the row did not remember being a chat (persona=${JSON.stringify(mine.persona)})`)
          chatTab.click()
          await new Promise((r) => setTimeout(r, 400))
          if ((await personaOf()) !== 'chat') {
            pageLog('selftest note: this core refused chat mode, so the chat list was not drawn')
          } else {
            const side = document.querySelector('.sidebar') ?? document.body
            const txt = side.textContent ?? ''
            if (!txt.includes('Chats')) fail('chat mode draws no Chats list - the sidebar is still a folder tree')
            if (txt.includes('Workspaces')) fail('chat mode still draws the workspace tree beside the chats')
            const titles = [...side.querySelectorAll('.sess-title')].map((e) => e.textContent)
            if (!titles.includes('selftest chat list'))
              fail(`the chat list does not contain the chat it made: ${JSON.stringify(titles.slice(0, 8))}`)
            const main = document.querySelector('main') ?? document.body
            if (!(main.textContent ?? '').includes('Chat has no folder'))
              fail('the composer still offers a folder line where a chat owns none')
            if (
              !document.querySelector('[data-approvals="none"]') ||
              document.querySelector('[data-approvals]:not([data-approvals="none"])')
            )
              fail('the approvals control is still offered in chat mode, where nothing can need approving')
            // A mode switch resets the view, so the check has to give it a view to reset:
            // open the chat the way a person does, by clicking its row. Without that the
            // claim below would be vacuous — switching with nothing on screen looks correct
            // whatever the code does.
            const chatRow = [...side.querySelectorAll('.sessbtn')].find((b) =>
              (b.textContent ?? '').includes('selftest chat list'),
            ) as HTMLElement | undefined
            if (!chatRow) fail('the chat row in the sidebar has no button to open it')
            else {
              chatRow.click()
              await renderTick()
              if (viewId !== chatSess)
                fail(`clicking the chat row did not put it in the view (viewId=${viewId})`)
            }
            // Back to cowork: the chat must not reappear in a folder. It has one listed
            // nowhere, and a row that shows up in both lists is two lies.
            workTab.click()
            await new Promise((r) => setTimeout(r, 400))
            if ((await personaOf()) !== 'cowork') {
              fail(`Cowork did not restore the host (it reports ${await personaOf()})`)
            } else if (viewId !== null)
              // Cowork and Chat are different kinds of conversation, so the switch lands on
              // the screen the app opens on — the cup and the composer — rather than leaving
              // the reader inside the other mode's last turn. What is *not* deleted is the
              // conversation itself: a mode switch is a change of view, not an edit.
              fail(
                `switching mode left the view parked on session ${viewId}; a mode is a new kind of conversation and belongs on the start screen`,
              )
            else if (!document.querySelector('.empty .cup'))
              fail('after a mode switch the start screen is not on screen (no cup)')
            else if (!(await invoke<Session[]>('list_sessions')).some((s) => s.id === chatSess))
              fail('the mode switch destroyed the conversation it switched away from')
            else {
              const side2 = document.querySelector('.sidebar') ?? document.body
              const t2 = [...side2.querySelectorAll('.sess-title')].map((e) => e.textContent)
              if (t2.includes('selftest chat list'))
                fail('a chat showed up in the workspace list once the tab said Cowork')
              if (!document.querySelector('[data-approvals]'))
                fail('the approvals control did not come back with Cowork')
              pageLog(`chat list ok (${titles.length} chat row(s) drawn, no folder tree, no approvals control)`)
            }
          }
        } catch (e) {
          fail(`chat list check: ${e}`)
        } finally {
          await invoke('delete_session', { id: chatSess }).catch(() => {})
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
          await frame(0, 2)
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
        await invoke('set_reasoning', { level: 'off', persist: true })
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
      await invoke('set_reasoning', { level: 'medium', persist: true })

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
        // ...and it must be there exactly once. The store holds the user's own message
        // from the moment the turn was sent (crates/bridge/src/host.rs writes the row
        // before the model is asked, so a message survives a crash mid-turn), which means
        // the history this repaint just loaded already contained the row that `sentDraft`
        // is a copy of. Painting both stamped the same sentence on the screen twice, which
        // is the report: "switch from an active session to another and back, and the first
        // turn's user message is duplicated."
        const mineRows = [...(rootEl?.querySelectorAll('.blk-user') ?? [])]
        if (mineRows.length !== 1)
          fail(
            `the turn in flight shows ${mineRows.length} copies of the message you sent; want exactly 1 (${mineRows
              .map((u) => JSON.stringify(u.textContent?.slice(0, 24)))
              .join(' | ')})`,
          )
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

      // --- a turn that outlived the tab that started it ---------------------
      // The phone report: background the browser during a turn, come back, and nothing
      // is typeable — "I can't interact with any input, especially the composer, and I
      // always have to refresh."
      //
      // The composer is `disabled={busy}`, and `busy` was stuck true: the `turnEnd` that
      // would clear it was delivered to a frozen page and is gone, and `turnOurs` made
      // every later read of host state get thrown away before it could correct anything.
      // The harness cannot background a webview, so the *state* a background leaves
      // behind is manufactured here and handed to the same reconciliation the wake
      // handler runs.
      if (want('outlived-tab', 'a turn that outlived the tab that started it')) {
        const hNow = await invoke<HostState>('session_state')
        if (hNow.busy) {
          pageLog('selftest note: skipped the stale-turn check; the host is busy with something else, and its answer would not be idle')
        } else {
          busy = true
          turnOurs = true
          turnSentAt = performance.now() - 90_000
          await refreshState(true)
          if (busy)
            fail(
              'a forced host read still trusted a 90s-old claim that this client was mid-turn: the composer stays dead',
            )
          if (turnOurs) fail('the stale "this turn is mine" claim survived the forced read')

          // The guard has to face the other way as well. A send that left this client a
          // moment ago may still be crossing the network, and the host is right to report
          // "not busy yet" about it; believing that would re-enable the composer mid-send.
          busy = true
          turnOurs = true
          turnSentAt = performance.now()
          await refreshState(true)
          if (!busy) fail('a send made a moment ago was treated as finished')
          busy = false
          turnOurs = false
          turnSentAt = 0
        }
      }

      // --- two dims, two names ---------------------------------------------
      // The sidebar drawer dims the app behind it, and the settings screen dims the app
      // behind it. They used to be the same class: the settings rule sits later in the
      // stylesheet at the same specificity, so it won the cascade for `position` and
      // `z-index`, and the drawer's dim became `fixed` at z-index 70 — above the drawer
      // it is meant to sit behind (40). Every touch on an open drawer landed on the dim,
      // whose entire job is to close it. That is the report: "in mobile view the sidebar's
      // content is unclickable; I tap and it just hides, and I have to zoom the page out
      // until the sidebar docks so I can use it."
      if (want('dims', 'two dims, two names')) {
        setSidebar(true)
        await renderTick()
        const dim = document.querySelector('button.scrim') as HTMLElement | null
        if (!dim) fail('the sidebar drawer has no dim to tap away')
        else {
          const cs = getComputedStyle(dim)
          const dimZ = Number(cs.zIndex)
          if (!Number.isFinite(dimZ) || dimZ >= 40)
            fail(
              `the drawer's dim sits at z-index ${dimZ}, at or above the drawer's own 40: it covers the session list and eats its taps`,
            )
          // Above 768px there is no drawer, so there is nothing to dim. A dim mounted there
          // covers a docked sidebar, which has no z-index of its own to answer it with -
          // which is how Ctrl+B made the session list unclickable on the desktop too.
          if (window.innerWidth >= 768 && cs.display !== 'none')
            fail('the drawer dim is mounted at desktop width, where it sits over a docked sidebar and blocks it')
        }
        setSidebar(false)
        openSettings('models')
        await renderTick()
        const mdim = document.querySelector('.mscrim') as HTMLElement | null
        const panel = document.querySelector('.settings') as HTMLElement | null
        if (!mdim || !panel)
          fail('the settings overlay has no dim of its own (.mscrim): it must not share the drawer dim\'s class')
        else {
          const mZ = Number(getComputedStyle(mdim).zIndex)
          const pZ = Number(getComputedStyle(panel).zIndex)
          if (!(mZ > 40)) fail(`the settings dim sits at z-index ${mZ}; it has to clear the drawer's 40`)
          if (!(pZ > mZ)) fail(`the settings panel (${pZ}) is not above its own dim (${mZ})`)
          // A dim that paints over the app is not the same as an app you cannot reach. The
          // report was that the status bar under the dialog read as live — the metrics and
          // gateway controls stayed visible at 26% black and a click there did something
          // other than what was aimed at. So: the dim covers the window, the click over a
          // background control lands on the dim, and the background itself is inert.
          if (offScreen()) {
            pageLog(`selftest note: the settings frame and what is behind it were not measurable here; ${offScreen()}`)
          } else {
          const dr = mdim.getBoundingClientRect()
          if (dr.width < window.innerWidth - 1 || dr.height < window.innerHeight - 1)
            fail(`the settings dim covers ${Math.round(dr.width)}x${Math.round(dr.height)} of a ${window.innerWidth}x${window.innerHeight} window`)
          const under = document.querySelector('.side-id [data-menu]') as HTMLElement | null
          if (!under) fail('the sidebar footer row has no control to test the dim against')
          else {
            const ub = under.getBoundingClientRect()
            const hit = document.elementFromPoint(ub.x + ub.width / 2, ub.y + ub.height / 2)
            if (hit && !hit.classList.contains('mscrim'))
              fail(`a click over the sidebar footer row while settings is open lands on .${String(hit.className || hit.tagName)}, not on the dim`)
            if (!document.querySelector('.app-shell.modal-open'))
              fail('the app shell does not mark a modal open, so nothing behind the dialog is taken out of the tab order')
            // `inert` is a property on current engines and nothing at all on older ones; the
            // CSS rule is the half that works everywhere, so either answer is a pass and a
            // third one (still interactive) is the bug.
            const inertBehind = (under as any).inert === true || getComputedStyle(under).pointerEvents === 'none'
            if (!inertBehind)
              fail('the sidebar footer row behind the settings dialog is still interactive (neither inert nor pointer-events: none)')
          }
          // The frame belongs to the dialog, not to whichever page happens to be open.
          // It used to be `max-height`, so the short pages collapsed it and the long ones
          // grew it, and every page change moved the controls out from under the pointer.
          const first = panel.getBoundingClientRect()
          if (getComputedStyle(panel).resize === 'none')
            fail('the settings dialog has no resize handle; the person cannot make it bigger')
          gotoSettings('appearance')
          await renderTick()
          const second = panel.getBoundingClientRect()
          if (Math.abs(second.height - first.height) > 2 || Math.abs(second.width - first.width) > 2)
            fail(
              `changing settings page resized the dialog: ${Math.round(first.width)}x${Math.round(first.height)} to ${Math.round(second.width)}x${Math.round(second.height)}`,
            )
          else
            pageLog(
              `settings frame is the dialog's own (${Math.round(first.width)}x${Math.round(first.height)}, same on two pages of wildly different length, resizable, background inert)`,
            )
          }
        }
        closeSettings()
        await renderTick()
        const stillInert = document.querySelector('.side-id [data-menu]') as HTMLElement | null
        if (stillInert && (stillInert as any).inert === true)
          fail('the sidebar footer row stayed inert after settings closed: the app would be unusable with no dialog on screen')
      }

      // --- the status bar fits the window it is given -------------------------
      //
      // ---- what stays pinned in the sidebar ---------------------------------
      //
      // Two things in the sidebar are chrome, not content, and both used to scroll: the "Workspaces"
      // heading (which carries fold-all and add-a-folder, so losing it mid-list means losing the
      // controls, not just a label) and the identity row that replaced the bottom toolbar. A heading
      // you can only reach by scrolling to the top of the list is a heading that hides the controls it
      // owns. And the row at the foot is where telemetry, the gateway and the theme live now, so if it
      // scrolls, the app loses its switches in the middle of a long conversation list.
      if (want('sidebar-chrome', 'the sidebar keeps its heading and its identity row pinned')) {
        const side = document.querySelector('aside.sidebar') as HTMLElement | null
        const list = side?.querySelector('.sesslist') as HTMLElement | null
        const head = side?.querySelector('.sesshead') as HTMLElement | null
        const row = side?.querySelector('[data-side-id]') as HTMLElement | null
        if (!side || !list || !head || !row)
          fail(`the sidebar is missing a part: aside=${!!side} list=${!!list} heading=${!!head} identity row=${!!row}`)
        else if (await blind('the sidebar pinning check')) {
          /* rects would all read 0 and the failures would read like CSS bugs */
        } else {
          const bad: string[] = []
          if (list.contains(head))
            bad.push('the Workspaces heading is inside the scrolling list, so it scrolls away with the conversations')
          if (list.contains(row))
            bad.push('the identity row is inside the scrolling list, so the switches scroll away')

          // Scroll the list to its far end and ask whether the chrome is still where it was.
          const maxScroll = list.scrollHeight - list.clientHeight
          list.scrollTop = maxScroll
          await renderTick()
          const sr = side.getBoundingClientRect()
          const hr = head.getBoundingClientRect()
          const rr = row.getBoundingClientRect()
          const lr = list.getBoundingClientRect()
          if (maxScroll > 8 && list.scrollTop < 8)
            bad.push(`the session list would not scroll (${list.scrollTop} of ${maxScroll}px): the check has nothing to prove`)
          if (hr.top < sr.top - 1 || hr.bottom > sr.bottom + 1)
            bad.push(`the heading left the sidebar while the list was scrolled to its end (${Math.round(hr.top)}..${Math.round(hr.bottom)} against ${Math.round(sr.top)}..${Math.round(sr.bottom)})`)
          if (rr.top < sr.top - 1 || rr.bottom > sr.bottom + 1)
            bad.push(`the identity row left the sidebar while the list was scrolled (${Math.round(rr.top)}..${Math.round(rr.bottom)} against ${Math.round(sr.top)}..${Math.round(sr.bottom)})`)
          // Rows must not be able to travel underneath the heading: the scroller starts below it.
          if (lr.top < hr.bottom - 1)
            bad.push(`the list can scroll rows underneath the heading (list starts at ${Math.round(lr.top)}, the heading ends at ${Math.round(hr.bottom)})`)
          const add = head.querySelector('.ws-addall') as HTMLElement | null
          if (!add) bad.push('the pinned heading carries no add-a-folder control, so pinning it bought nothing')
          else {
            const ar = add.getBoundingClientRect()
            if (ar.right > sr.right + 1 || ar.left < sr.left - 1)
              bad.push('the add-a-folder control in the pinned heading hangs off the sidebar edge')
          }
          // The row has to say who it belongs to, not just hold three buttons.
          if (!row.querySelector('img')) bad.push('the identity row has no mark')
          if (!(row.textContent ?? '').includes('Cwoffee')) bad.push('the identity row does not name the app')
          list.scrollTop = 0
          await renderTick()
          if (bad.length) bad.forEach((b) => fail(b))
          else
            pageLog(
              `sidebar chrome pinned ok ("${head.textContent?.trim().slice(0, 12)}" and the identity row sit outside the ${Math.round(maxScroll)}px of scrollable list; the list starts ${Math.round(lr.top - hr.bottom)}px below the heading, and the row still holds its mark, its name and 3 controls)`,
            )
        }
      }

      // "The footer items are too tiny and too long: on my phone the theme and help buttons are
      // off-screen." That bar is gone, but the failure mode it was measured for did not go away with
      // it: the sidebar footer row is the new pinned strip of controls, and on a phone the sidebar is
      // an overlay narrower than this one. So the same question gets asked of the row that replaced the
      // bar - narrow the sidebar in place and ask whether every control still fits inside it, and is
      // still big enough for a thumb. The name is allowed to ellipsize; the controls are not allowed to
      // shrink, move, or fall off.
      if (want('statusbar', 'the sidebar footer row fits the window it is given')) {
        const row = document.querySelector('[data-side-id]') as HTMLElement | null
        const side = document.querySelector('aside.sidebar') as HTMLElement | null
        if (!row || !side) fail('the sidebar has no identity row to measure')
        else if (document.querySelector('footer')) fail('the bottom toolbar is still on the page')
        else if (await blind('the sidebar footer row fit check')) {
          /* rects would all read 0 and the failures would read like CSS bugs */
        } else {
          const wanted = '208px'
          const prev = side.style.width
          side.style.width = wanted
          await renderTick()
          const rr = row.getBoundingClientRect()
          const sr = side.getBoundingClientRect()
          const controls = [...row.querySelectorAll('button')] as HTMLElement[]
          const shown = controls.filter((c) => c.getBoundingClientRect().width > 0)
          const outside = shown.filter((c) => {
            const r = c.getBoundingClientRect()
            return r.right > rr.right + 1 || r.left < rr.left - 1 || r.right > sr.right + 1
          })
          const tiny = shown.filter((c) => {
            const r = c.getBoundingClientRect()
            return r.height < 28 || r.width < 24
          })
          const names = (xs: HTMLElement[]) =>
            xs.map((c) => c.getAttribute('aria-label') ?? String(c.className)).join(', ')
          const labels = controls.map((c) => c.getAttribute('aria-label') ?? '?')
          side.style.width = prev
          await renderTick()
          if (controls.length !== 3)
            fail(
              `the sidebar footer row holds ${controls.length} control(s) (${labels.join(', ')}); it holds exactly three: telemetry, gateway, theme. ipc, search and the shortcut sheet belong in Settings`,
            )
          else if (!shown.length) fail(`at ${wanted} of sidebar every control is hidden: a narrow window has no controls at all`)
          else if (outside.length) fail(`at ${wanted} of sidebar, ${outside.length} control(s) sit outside the row: ${names(outside)}`)
          else if (tiny.length) fail(`at ${wanted} of sidebar, ${tiny.length} control(s) are smaller than a thumb: ${names(tiny)}`)
          else
            pageLog(
              `sidebar footer row fits ${wanted} of sidebar (${labels.join(', ')}; all inside a ${Math.round(rr.width)}px row, each at least 28px tall)`,
            )
        }
      }

      // --- draft sessions ----------------------------------------------------
      // "New session" must not leave an empty row behind, and the row must
      // appear only when the first message is sent - already titled by it.
      if (want('drafts', 'draft sessions')) {
        const beforeDraft = await invoke<Session[]>('list_sessions')
        newSession()
        await new Promise((r) => setTimeout(r, 300))
        const afterDraft = await invoke<Session[]>('list_sessions')
        if (afterDraft.length !== beforeDraft.length) {
          fail(`new session created a row: ${beforeDraft.length} -> ${afterDraft.length}`)
        }
        if (!showEmpty) fail('draft view did not present the empty state')
        // The header `+` sits in line with the mode switch and asks for a conversation with no
        // folder picked. It used to keep whichever folder the previous draft was aimed at, so
        // the chip named a folder this conversation was never given - and when that folder was
        // the scratch one, it named it by path, which reads like a choice somebody made.
        // Seeded with the host's own folder first: the assertion below is about the value
        // surviving, and seeding keeps the rest of this check behaving the same either way.
        draftWorkspace = hostState?.workspace ?? ''
        newSession()
        await renderTick()
        if (draftWorkspace)
          fail(
            `the header + carried the folder ${JSON.stringify(draftWorkspace)} over from the previous draft`,
          )
        else if (isTempRoot(hostState?.workspace ?? '')) {
          const chip = document.querySelector('.wsrow')?.textContent ?? ''
          if (!chip.includes('No workspace'))
            fail(`the host sits in the scratch folder and the composer chip says ${JSON.stringify(chip.trim())}`)
        }

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
          const known = new Set(beforeDraft.map((x) => x.id))
          const created = afterSend.filter((x) => !known.has(x.id))
          if (created.length !== 1) {
            fail(`first message did not create exactly one row: ${beforeDraft.length} -> ${afterSend.length}`)
          } else {
            // The row used to be named by cutting this prompt at 48 characters, which is how
            // the sidebar filled with "What is 6 times 7" and "hey can you". It is named by
            // the model now - so the claim here is that the placeholder was replaced by the
            // mock's title, and that nothing cut from the prompt is what a row is called.
            const mine = created[0]
            if (mine.title === 'New Session') {
              fail('the conversation is still holding its placeholder after its first turn')
            } else if (mine.title !== 'Selftest title, typed in') {
              fail(`the model did not name the new row: ${JSON.stringify(mine.title)}`)
            } else if (afterSend.some((x) => x.title.includes('6 times 7'))) {
              fail('a row is still titled by cutting the opening line')
            } else {
              await invoke('delete_session', { id: mine.id })
            }
          }
          if (!(await waitFor('view to leave the empty state', () => !showEmpty, 5000))) {
            fail(`view stayed empty after sending (viewId=${viewId} msgCount=${viewMsgCount} busy=${busy})`)
          }
        }
      }

      // --- the search panel reports the engine, and never changes the choice ----
      // --- the search page reports the engine, and never changes the choice ----
      //
      // The search pill went away with the bottom toolbar, so this drives the surface the fact lives on
      // now: Settings -> Search. The claims are unchanged - the provider list is complete, exactly one
      // row is marked, the marked row agrees with the engine rather than with a local default, and the
      // harness never clicks a provider, because choosing one writes the person's config file.
      if (want('search', 'the search page reports the engine, and never changes the choice')) {
        gotoSettings('workspace')
        await renderTick()
        const sect = document.querySelector('[data-set-section="search"]') as HTMLElement | null
        if (!sect) {
          fail('settings has no search section, so a phone cannot see which search is on')
        } else {
          // The page reads its state on first open, including a container probe
          // (podman ps), so give the round trip room.
          if (!(await waitFor('the search section to fill in', () => document.querySelectorAll('[data-set-section="search"] [role="radio"]').length >= 3, 4000))) {
            fail('the search page opened but never listed its providers')
          } else {
            // The page body is the same snippet the old popover rendered, so the checks
            // below name the row kinds, not one surface's class names.
            const items = [...document.querySelectorAll('[data-set-section="search"] [role="radio"]')] as HTMLElement[]
            const labels = items.map((i) => (i.querySelector('.schoice-l')?.textContent ?? '').trim())
            const want = ['DuckDuckGo', 'SearXNG, run by Cwoffee', 'SearXNG, already running']
            const missing = want.filter((w) => !labels.includes(w))
            if (missing.length)
              fail(`the search section is missing provider(s): ${missing.join(' | ')} (saw ${labels.join(' | ')})`)
            const marked = items.filter((i) => i.getAttribute('aria-checked') === 'true')
            if (marked.length !== 1)
              fail(`exactly one provider must be marked as in use, saw ${marked.length}: ${labels.join(' | ')}`)
            const inUse = document.querySelector('[data-set-section="search"] [data-web-current]') as HTMLElement | null
            const said = (inUse?.querySelector('.srow-h')?.textContent ?? '').trim()
            if (!inUse || said.length < 6)
              fail(
                `the search section does not say what search is actually doing (got ${said || 'nothing'}): a card that prints the config value cannot tell "auto" with a container from "auto" with no runtime`,
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
            const panel = document.querySelector('[data-set-section="search"] .ssect-b') as HTMLElement | null
            if (!panel) fail('the search section body vanished mid-check')
            else {
              // A settings section lives in a scrolling body, so "is it inside the window" is not a
              // question that means what it meant for the floating popover this check used to drive -
              // that one was always placed on screen. What still means something here is that the
              // section fits its dialog sideways, and that its rows do not sit on each other.
              sect.scrollIntoView({ block: 'nearest' })
              await frame(2)
              const pr = panel.getBoundingClientRect()
              const dlg = document.querySelector('.settings')?.getBoundingClientRect()
              if (pr.width < 200 || pr.height < 60)
                fail(`the search section has no size: ${pr.width.toFixed(0)}x${pr.height.toFixed(0)}`)
              if (dlg && (pr.left < dlg.left - 1 || pr.right > dlg.right + 1))
                fail(
                  `the search section is wider than the dialog it is in: section ${pr.left.toFixed(0)}..${pr.right.toFixed(0)}, dialog ${dlg.left.toFixed(0)}..${dlg.right.toFixed(0)}`,
                )

            const overlaps = (a: Element, b: Element) => {
              const x = a.getBoundingClientRect(),
                y = b.getBoundingClientRect()
              return (
                Math.min(x.right, y.right) - Math.max(x.left, y.left) > 4 &&
                Math.min(x.bottom, y.bottom) - Math.max(x.top, y.top) > 4
              )
            }
            const rows = [...panel.querySelectorAll('.srow, .schoice')] as HTMLElement[]
            for (let i = 0; i + 1 < rows.length; i++)
              if (overlaps(rows[i], rows[i + 1]))
                fail(`provider rows overlap: "${rows[i].textContent?.trim().slice(0, 20)}" / "${rows[i + 1].textContent?.trim().slice(0, 20)}"`)

            const synth = document.createElement('div')
            synth.className = 'srow'
            const wrap = document.createElement('div')
            const l1 = document.createElement('div')
            l1.className = 'srow-t'
            l1.textContent = 'Its own container'
            const l2 = document.createElement('div')
            l2.className = 'srow-h'
            l2.textContent = 'podman · 8899 · idle in 120s · standing up'
            const l3 = document.createElement('div')
            l3.className = 'srow-h mono'
            l3.textContent = 'port 8899 · idle 120s · config /home/user/.config/cwoffee/searxng'
            wrap.append(l1, l2, l3)
            synth.append(wrap)
            panel.appendChild(synth)
            await frame(60)
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
                  `(the two helper lines of a settings row must stack, not share a line)`,
              )
            const real = panel.querySelector('[data-web-auto]')
            pageLog(
              `search page geometry ok (${rows.length} rows, ${pr.width.toFixed(0)}x${pr.height.toFixed(0)} inside ` +
                `${innerWidth}x${innerHeight}, status block stacks, live status ${real ? 'shown' : 'not applicable on this backend'})`,
            )
            }
            closeSettings()
            await new Promise((r) => setTimeout(r, 150))
            pageLog(`search page ok (${view.backend}: ${view.describing})`)
          }
        }
      }

      // --- the conversation names itself, before it answers ------------------
      // The old titler borrowed a second model, ran it *after* the first turn, and cost
      // about 870 MB to do it. What replaced it is one short request to the endpoint that
      // is already answering, made before the answer, in its own context, with the name
      // typed into the sidebar row as it arrives. Four things are asserted here, because
      // they are four things a reader can see go wrong: the row starts as the
      // placeholder; the name arrives with a caret rather than swapping in; the settled
      // row carries the model's words; and the ask never becomes part of the
      // conversation it named.
      if (want('titling', 'the conversation names itself before it answers')) {
        const sid = await invoke<number>('new_session', { title: 'New Session' })
        await loadSessions()
        await new Promise((r) => setTimeout(r, 300))
        const started = sessions.find((s) => s.id === sid)?.title ?? ''
        if (started !== 'New Session') {
          fail(`a new conversation does not start as the placeholder: ${JSON.stringify(started)}`)
        } else {
          // Open it before sending, so "the last turn on screen" is this conversation's turn.
          // Without that, `.turn:last-child` belongs to whatever was up before, the answer clock
          // starts before the send, and the ordering claim cannot be falsified.
          openSession(sid)
          await new Promise((r) => setTimeout(r, 400))
          const turnsBefore = hostState?.turns ?? 0
          let caretSeen = 0
          let caretFirst = 0
          let answerFirst = 0
          // Scoped to this conversation's own row: a document-wide `.sess-title` query can
          // grade another conversation's row, and a folded folder can hide this one's.
          const rowTitle = () => document.querySelector(`.sess-title[data-sess="${sid}"]`)
          // The ordering claim is about the shipped signal, not about when pixels appeared: a
          // turn can paint its first word in one batch on a page that gets a single frame. So
          // the clock that matters is "when did the page learn a turn had started" (`busy`, which
          // TurnStart sets) next to "when was the name being typed".
          let busyFirst = 0
          const poll = setInterval(() => {
            const now = performance.now()
            if (rowTitle()?.querySelector('.type-caret')) {
              caretSeen += 1
              if (!caretFirst) caretFirst = now
            }
            if (!busyFirst && busy) busyFirst = now
            const a = document.querySelector('.transcript .blk-answer .blk-body')
            if (!answerFirst && a && (a.textContent ?? '').trim()) answerFirst = now
          }, 10)
          await invoke('start_turn', { text: 'What is 6 times 7?', attachments: [], session: sid })
          const done = await waitFor(
            'the titling turn to finish',
            () => (hostState?.turns ?? 0) > turnsBefore,
            20000,
          )
          clearInterval(poll)
          if (!done) {
            fail('the titling turn never finished')
          } else {
            await loadSessions()
            await new Promise((r) => setTimeout(r, 300))
            const settled = sessions.find((s) => s.id === sid)?.title ?? ''
            const page = await invoke<{ messages: { content?: string }[] }>('session_messages', { id: sid })
            const leaked = page.messages.some((m) => (m.content ?? '').includes('Title this conversation in under'))
            const shown = !!rowTitle() && (rowTitle()!.textContent ?? '').includes('Selftest title')
            if (settled !== 'Selftest title, typed in') {
              fail(`the model's title never landed on the row: ${JSON.stringify(settled)}`)
            } else if (leaked) {
              fail('the title ask became part of the conversation it names')
            } else if (!shown) {
              fail('the sidebar row does not show the name the model gave')
            } else if (!caretSeen) {
              fail('the name swapped into the row instead of typing itself in: no caret was ever painted')
            } else if (!namingFirstAt || !turnStartTs) {
              fail(
                `no clock for the ordering claim: name arrived at ${namingFirstAt.toFixed(0)}ms, ` +
                  `turn start recorded at ${turnStartTs.toFixed(0)}ms`,
              )
            } else if (namingFirstAt > turnStartTs) {
              fail(
                `the name was still arriving ${(namingFirstAt - turnStartTs).toFixed(0)}ms after the turn had started`,
              )
            } else {
              pageLog(
                `titling ok (row started as "New Session"; the name typed itself in over ${caretSeen} ` +
                  `poll(s), first name paint ${namingFirstAt.toFixed(0)}ms against the turn's start ` +
                  `${turnStartTs.toFixed(0)}ms${answerFirst ? `, first answer text ${answerFirst.toFixed(0)}ms` : ''}; ` +
                  `settled as ${JSON.stringify(settled)}; the ask is not among the ${page.messages.length} stored row(s))`,
              )
            }
          }
        }
      }

      // --- delete through the actual DOM, the way a user does it ------------
      // The conversation deleted here is the one on screen, which is the case that used
      // to break. The store half was always right - the Rust command moves the host off
      // the row before removing it - but the screen kept painting a conversation that no
      // longer existed, with the composer still addressed at it, so the next message went
      // at an id the engine could not load.
      if (want('delete', 'delete through the actual DOM, the way a user does it')) {
        const doomed = await invoke<number>('new_session', { title: 'New Session' })
        const turnsBefore = hostState?.turns ?? 0
        await invoke('start_turn', { text: 'What is 6 times 7?', attachments: [], session: doomed })
        if (
          !(await waitFor(
            'the doomed conversation to take a turn',
            () => (hostState?.turns ?? 0) > turnsBefore,
            20000,
          ))
        ) {
          fail('the doomed conversation never took its turn')
        } else {
          // Name it by hand so the row is findable by a unique string: every titled
          // conversation in this run carries the mock's title.
          await invoke('rename_session', { id: doomed, title: 'selftest doomed' })
          await loadSessions()
          await new Promise((r) => setTimeout(r, 300))
          // Folders fold, and a closed folder renders no rows for the conversations inside it.
          // That is the list behaving, not failing - so press the sidebar's own unfold control
          // before looking for a row to click.
          const unfold = document.querySelector('.iconbtn.ws-fold[aria-label="Unfold every folder"]') as HTMLElement | null
          if (unfold) {
            unfold.click()
            await new Promise((r) => setTimeout(r, 300))
          }
          const row = [...document.querySelectorAll('.sessrow')].find((r) =>
            (r.textContent ?? '').includes('selftest doomed'),
          )
          if (!row) {
            const rows = [...document.querySelectorAll('.sessrow')]
            fail(
              `the doomed session row was not rendered. Host rows: ${sessions
                .map((s) => `${s.id}:${JSON.stringify(s.title)}:${s.workspace || 'none'}`)
                .join(' | ')}. Rendered .sessrow: ${rows.length} -> ${rows
                .map((r) => JSON.stringify((r.textContent ?? '').trim().slice(0, 30)))
                .join(' | ')}`,
            )
          } else {
            // Open it first: the premise of this check is a loaded transcript.
            (row.querySelector('.sessbtn') as HTMLElement | null)?.click()
            // Wait for history to paint rather than sleeping and hoping: the rows arrive
            // asynchronously, and a check that asserts its own premise on a fixed delay fails
            // on a slow disk instead of failing on the feature.
            // Blocks, not `.turn`: a stored conversation is rendered as its blocks at the
            // transcript root (`.blk-user`, `.blk-answer`, `.turn-acts.from-history`); the
            // `.turn` wrapper belongs to a turn the page is watching arrive live.
            const blocksOnScreen = () =>
              document.querySelectorAll('.transcript .blk, .transcript .turn').length
            const loaded = await waitFor(
              "the doomed conversation's history to paint",
              () => blocksOnScreen() > 0,
              4000,
            )
            const onScreen = blocksOnScreen()
            if (!loaded || onScreen === 0) {
              const tr = document.querySelector('.transcript')
              const kids = tr ? [...tr.children].slice(0, 6).map((c) => c.className || c.tagName) : []
              fail(
                `the doomed conversation would not load into the transcript (viewId=${viewId}, ` +
                  `rows the host handed back=${viewMsgCount}, .transcript mounted=${!!tr}, ` +
                  `rect=${tr ? `${Math.round(tr.getBoundingClientRect().width)}x${Math.round(tr.getBoundingClientRect().height)}` : 'none'}, ` +
                  `.turn anywhere=${document.querySelectorAll('.turn').length}, ` +
                  `empty screen=${!!document.querySelector('.empty')}, children=${kids.join(' | ')}) - ` +
                  'so this check has no premise',
              )
            } else {
              const dots = row.querySelector('.iconbtn.more') as HTMLElement | null
              if (!dots) {
                fail('no 3-dots control on the session row')
              } else {
                dots.click()
                await new Promise((r) => setTimeout(r, 350))
                const items = () => [...document.querySelectorAll('.pop .pop-item')] as HTMLElement[]
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
                    if (still.some((x) => x.id === doomed)) {
                      fail('session survived the delete flow')
                    } else {
                      await new Promise((r) => setTimeout(r, 500))
                      const left = blocksOnScreen()
                      if (left !== 0) {
                        fail(
                          `${left} block(s) still painted after deleting the conversation that was on screen: ` +
                            'the transcript did not let go of a conversation that no longer exists',
                        )
                      } else if (viewId !== null) {
                        fail(`the view still names conversation ${viewId}, which has been deleted`)
                      } else {
                        pageLog(
                          `delete ok (a loaded conversation of ${onScreen} turn block(s) was deleted; the screen let ` +
                            'go of it and the composer is back on the start screen)',
                        )
                      }
                    }
                  }
                }
              }
            }
          }
        }
      }

      // --- picking in the sidebar: ctrl-click, shift for a run, then bulk delete --
      // The gesture is invisible, so it is tested through the DOM the way a user does
      // it, and the final assertion is against the store: what a bulk delete promises to
      // remove has to actually be gone.
      if (want('selection', 'picking in the sidebar: ctrl-click, shift for a run, then bulk delete')) {
        const rowOf = (title: string) =>
          [...document.querySelectorAll('.sessrow')].find((r) =>
            r.textContent?.includes(title),
          ) as HTMLElement | undefined
        const clickRow = (el: HTMLElement, mods: { ctrlKey?: boolean; shiftKey?: boolean }) => {
          const btn = (el.querySelector('.sessbtn') ?? el) as HTMLElement
          btn.dispatchEvent(
            new MouseEvent('click', {
              bubbles: true,
              cancelable: true,
              ctrlKey: !!mods.ctrlKey,
              shiftKey: !!mods.shiftKey,
            }),
          )
        }
        const gone = async (ids: number[]) => {
          const rest = await invoke<Session[]>('list_sessions')
          return ids.filter((id) => rest.some((x) => x.id === id))
        }
        const pa = await invoke<number>('new_session', { title: 'selftest pick a' })
        const pb = await invoke<number>('new_session', { title: 'selftest pick b' })
        await loadSessions()
        await new Promise((r) => setTimeout(r, 300))
        const ra = rowOf('selftest pick a')
        const rb = rowOf('selftest pick b')
        if (!ra || !rb) {
          fail(`pick rows were not rendered (a=${!!ra} b=${!!rb})`)
        } else {
          // A plain click must still navigate. If it selects, the sidebar has changed
          // meaning and people will lose the thing they use it for.
          clickRow(ra, {})
          await new Promise((r) => setTimeout(r, 250))
          if (document.querySelectorAll('.picked').length) fail('a plain click selected a row')
          clickRow(ra, { ctrlKey: true })
          await new Promise((r) => setTimeout(r, 250))
          const n1 = document.querySelectorAll('.picked').length
          const boxes = document.querySelectorAll('.pickbox').length
          if (n1 !== 1 || !boxes)
            fail(`ctrl-click picked ${n1} row(s), drew ${boxes} checkbox(es); wanted 1 and some`)
          clickRow(rb, { shiftKey: true })
          await new Promise((r) => setTimeout(r, 250))
          const bar = document.querySelector('.bulkbar')
          if (!bar) {
            fail('no bulk bar after shift-clicking a second row')
          } else {
            const txt = (bar.textContent ?? '').replace(/\s+/g, ' ').trim()
            if (!/2 conversation/.test(txt))
              fail(`the bulk bar does not name what it will delete: "${txt}"`)
            const delBtn = () =>
              [...(bar.querySelectorAll('button') as NodeListOf<HTMLElement>)].find((x) =>
                /^(Delete|Confirm)$/.test((x.textContent ?? '').trim()),
              )
            const first = delBtn()
            if (!first) fail('the bulk bar has no Delete button')
            else {
              first.click()
              await new Promise((r) => setTimeout(r, 250))
              const second = delBtn()
              if (!second || (second.textContent ?? '').trim() !== 'Confirm') {
                fail(`first press did not arm the confirm (button says "${second?.textContent?.trim()}")`)
              } else {
                second.click()
                await new Promise((r) => setTimeout(r, 900))
                const left = await gone([pa, pb])
                if (left.length) fail(`bulk delete left ${left.length} conversation(s) in the store`)
                if (document.querySelectorAll('.picked').length)
                  fail('the basket stayed full after deleting')
                pageLog('picking ok (plain click navigates, ctrl-click picks, shift extends, confirm deletes for real)')
              }
            }
          }
        }
        // Tidy up whatever a failed branch left behind, so the scratch store stays clean
        // and the next run's row count means the same thing.
        for (const id of await gone([pa, pb])) await invoke('delete_session', { id }).catch(() => {})
      }

      // --- deleting a folder has to take the rows the folder *hides* with it -----------
      // The complaint on record: a workspace that looked empty could not be deleted, and
      // stayed in the sidebar "until all sessions get deleted". It looked empty because a
      // folder group shows neither chat conversations nor empty drafts, both of which are
      // filed under that folder's path in the store. Removing the registered path while any
      // of them remained was a no-op: the sidebar builds its folder list from the paths that
      // hold conversations, so the row came straight back. This case creates exactly that
      // shape - one visible conversation, one chat, one empty draft - picks the FOLDER row
      // only, and demands the store be empty of the folder afterwards.
      if (want('folder-delete', 'deleting a folder has to take the rows the folder *hides* with it')) {
        const home = hostState?.workspace ?? ''
        // The folder under test is the *parent* of the app's own workspace: it exists, it is
        // not the system temp folder (which the app refuses as a workspace, because that is
        // where no-workspace sessions live), and it holds nothing of anybody else's. The app
        // itself never files conversations there, so the rows this case creates are the only
        // contents, and "empty now" is a claim the assertions can actually make.
        const dir = home ? home.replace(/\/+$/, '').split('/').slice(0, -1).join('/') : ''
        const label = (t: string) =>
          [...document.querySelectorAll('.wsbtn')].find(
            (b) => (b.querySelector('.ws-name') as HTMLElement | null)?.getAttribute('title') === t,
          ) as HTMLElement | undefined
        const ctrlClick = (el: HTMLElement) =>
          el.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true, ctrlKey: true }))
        const made: number[] = []
        if (!dir || dir === '/' || dir === '/tmp') {
          fail(`no usable scratch folder (derived "${dir}" from workspace "${home}")`)
        }
        try {
          await invoke('add_workspace', { path: dir }).catch(() => {})
          const visible = await invoke<number>('new_session', { title: 'selftest folder visible', workspace: dir })
          made.push(visible)
          const chat = await invoke<number>('new_session', { title: 'selftest folder chat', workspace: dir })
          made.push(chat)
          await invoke('set_session_persona', { id: chat, persona: 'chat' })
          const draft = await invoke<number>('new_session', { title: 'selftest folder draft', workspace: dir })
          made.push(draft)
          // A conversation in the app's own folder, to prove the action is scoped to the one
          // picked. Named explicitly, because `new_session` without a workspace files the row
          // under wherever the host happens to be — and by now the host has been moved to the
          // scratch folder by the rows above, so an unnamed one would land inside the folder
          // being deleted and the scoping check would prove nothing.
          const elsewhere = await invoke<number>('new_session', {
            title: 'selftest folder elsewhere',
            workspace: home || undefined,
          })
          made.push(elsewhere)
          await loadSessions()
          await loadWorkspaces()
          await new Promise((r) => setTimeout(r, 300))

          const inside = await invoke<Session[]>('sessions_in', { path: dir })
          if (inside.length !== 3) {
            const what = inside.map((x) => `#${x.id} "${x.title}" ${x.persona ?? 'cowork'}`).join(' | ')
            fail(`sessions_in(${dir}) returned ${inside.length} row(s), wanted 3 (visible + chat + draft): ${what}`)
          }
          const row = label(dir)
          if (!row) fail(`no folder row for ${dir} to delete`)
          else {
            const shown = Number((row.querySelector('.ws-n')?.textContent ?? '').trim() || 0)
            ctrlClick(row)
            await new Promise((r) => setTimeout(r, 250))
            const bar = document.querySelector('.bulkbar')
            if (!bar) fail('picking a folder row drew no bulk bar')
            else {
              const txt = (bar.textContent ?? '').replace(/\s+/g, ' ').trim()
              // The basket counts what the store says is inside, not what the group shows.
              // `shown` is the number printed next to the folder, and it is smaller - that
              // difference is the bug the user reported, so the case asserts both.
              if (!/\b3 conversation/.test(txt))
                fail(`the basket counts from the visible list, not the folder's contents: "${txt}" (folder label showed ${shown})`)
              const delBtn = () =>
                [...(bar.querySelectorAll('button') as NodeListOf<HTMLElement>)].find((x) =>
                  /^(Delete|Confirm)$/.test((x.textContent ?? '').trim()),
                )
              const first = delBtn()
              if (!first) fail('the bulk bar has no Delete button')
              else {
                first.click()
                await new Promise((r) => setTimeout(r, 250))
                const armed = (document.querySelector('.toast')?.textContent ?? '').replace(/\s+/g, ' ')
                if (!/chat/.test(armed))
                  fail(`the confirm line hides the chat rows it is about to delete: "${armed.trim()}"`)
                const second = delBtn()
                if (!second || (second.textContent ?? '').trim() !== 'Confirm') {
                  fail(`first press did not arm the confirm (button says "${second?.textContent?.trim()}")`)
                } else {
                  second.click()
                  await new Promise((r) => setTimeout(r, 1200))
                  const still = await invoke<Session[]>('sessions_in', { path: dir })
                if (still.length)
                  fail(`${still.length} conversation(s) still filed under ${dir} after deleting the folder row`)
                const wss = await invoke<WorkspaceRow[]>('list_workspaces')
                if (wss.some((w) => w.path === dir))
                  fail(`${dir} is still registered after its last conversation was deleted`)
                if (label(dir)) fail(`${dir} still has a folder row in the sidebar`)
                const rest = await invoke<Session[]>('sessions_in', { path: home })
                if (!rest.some((x) => x.id === elsewhere))
                  fail(`the folder delete took a conversation filed under ${home} as well`)
                pageLog(
                  `folder delete ok (${shown} shown under the folder, ${inside.length} really inside, all gone, other folders untouched)`,
                )
                }
              }
            }
          }
        } catch (e) {
          fail(`folder delete case threw: ${cleanErr(e)}`)
        } finally {
          for (const id of made) {
            const live = await invoke<Session[]>('list_sessions')
            if (live.some((x) => x.id === id)) await invoke('delete_session', { id }).catch(() => {})
          }
          // A new_session with a workspace moves the host's folder. Put it back where the
          // rest of the board expects it, or every later case runs in /tmp.
          // The verb is `set_workspace` on both hosts. This used to call
          // `set_workspace_checked` — a name no host implements — behind a `.catch(() => {})`,
          // so the board left its scratch folder as the host's workspace and nothing said so.
          if (home && (await invoke<HostState>('session_state')).workspace !== home) {
            await invoke('set_workspace', { path: home })
            const back = (await invoke<HostState>('session_state')).workspace
            if (back !== home)
              pageLog(`folders: the board could not move the host back to ${home}; it is at ${back}`)
          }
          await loadSessions()
          await loadWorkspaces()
          // Leave no toast standing and no frame pending. This case speaks through the same
          // notice line every other case reads, and a bubble left up has been shown to swallow
          // the next case's click — which then reads as a regression in a feature two features
          // away.
          toast = null
          await new Promise((r) => setTimeout(r, 250))
          await frame(40)
        }
      }

      // --- the message you just sent must survive the session being created --
      // This is the video bug: sending from a draft wiped the user's own message
      // and opened on a blank transcript, because the new session announced
      // itself and the handler reloaded from a store that did not contain the
      // turn yet.
      if (want('first-message', 'the message you just sent must survive the session being created')) {
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
      if (want('live-marker', 'the live marker: one during the turn, none after')) {
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
      if (want('thinking-label', 'thinking label + markdown tables')) {
        await invoke('set_reasoning', { level: 'medium', persist: true })
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
            fail('no thinking line in the turn')
          } else {
            const head = rblk.querySelector('.blk-head') as HTMLElement | null
            if (!head) fail('the thinking line has no header')
            else {
              const label = head.textContent ?? ''
              if (!/^Thought for /i.test(label))
                fail(`thinking label was not replaced with a duration: ${JSON.stringify(label)}`)
              // The arrow wrapping onto its own line was the reported bug; now the line carries
              // a duration and a word count, so it has to stay one line to stay readable.
              if (head.getBoundingClientRect().height > 22)
                fail(`thinking head is not a single line: ${head.getBoundingClientRect().height}px`)
              // The preview is gone, and the ask was "indefinitely not accessible behind its
              // header": no body element at all, no expander, and nothing clickable.
              const bodies = rblk.querySelectorAll('.blk-body').length
              if (bodies) fail(`a thinking line carries ${bodies} body element(s); a thought has no body to open`)
              const pseudo = getComputedStyle(head, '::after').content
              if (pseudo !== 'none' && pseudo !== 'normal') fail('the thinking header still paints an expander')
              if (getComputedStyle(head).cursor !== 'default') fail('the thinking header still looks clickable')
              head.click()
              await new Promise((r) => setTimeout(r, 120))
              if (rblk.classList.contains('is-open') || head.classList.contains('open'))
                fail('clicking a thinking line opened something')
              const trun = rblk.closest('.blk-run') as HTMLElement | null
              if (!trun) fail('a thinking line in a live turn sits outside the run accordion')
              else {
                // "Show the working" reveals what the steps did. It must not bring the thinking
                // text back: this is the half of the removal a display setting could otherwise
                // undo, and the reason the check turns the setting on and reads the page again.
                transcript?.setVerbose(true)
                await new Promise((r) => setTimeout(r, 200))
                const extra = [...rblk.children].filter((n) => !n.matches('.blk-head')).length
                transcript?.setVerbose(false)
                await new Promise((r) => setTimeout(r, 200))
                if (extra) fail(`"show the working" put ${extra} element(s) back under a thinking line`)
              }
            }
          }
        }
      }

      // --- files a turn produces show up as clickable attachments ----------
      if (want('file-chips', 'files a turn produces show up as clickable attachments')) {
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
      if (want('code-chrome', 'code block chrome + what the preview sandbox actually blocks')) {
        await invoke('set_reasoning', { level: 'off', persist: true })
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

            // --- the header of a long block stays where the buttons are --------
            // A 200-line HTML or SVG answer used to leave its copy and preview buttons off
            // the top of the screen while the person was halfway through reading it. The fix
            // is one word wide (`overflow: clip` on the block instead of `hidden`, which had
            // made the block its own scroll container and therefore made `sticky` unable to
            // move), and the promise has two halves: the header holds at the top of the
            // transcript while the block is being read, and it goes away when the block ends
            // rather than floating over the next answer. So the check scrolls to the middle
            // of a tall block, then scrolls past its end.
            const sc = document.querySelector('.scroller') as HTMLElement | null
            if (!sc) fail('the transcript has no scroller for a code header to stick inside')
            else {
              const prevTop = sc.scrollTop
              // The fixture is the block the renderer actually produced, cloned and made
              // tall - so the class names and the nesting under test are the shipped ones,
              // not a hand-written guess of them. The filler after it is because otherwise
              // the block is the last thing in the document and "scrolled past its end" is
              // physically unreachable.
              const tall = blk.cloneNode(true) as HTMLElement
              const filler = document.createElement('div')
              filler.style.height = '600px'
              const pre = tall.querySelector('pre')
              if (pre)
                pre.textContent = Array.from({ length: 80 }, (_, i) => `row ${i} of a long answer`).join('\n')
              sc.append(tall, filler)
              const head = tall.querySelector('.code-head') as HTMLElement | null
              const copy = head?.querySelector('.code-act[data-act="copy"]') as HTMLElement | null
              if (!head || !copy) {
                fail('the cloned code block has no header to test')
              } else {
                const verdict: string[] = []
                // 1. Middle of the block: the header is at the top of the scroller and the
                //    buttons are inside the window. Measured with rectangles rather than
                //    `offsetTop`, because an offset is taken from the nearest positioned
                //    ancestor, which is not where the scroller's content begins - a check
                //    written against offsets lands the scroll in the wrong place and then
                //    reads the CSS as the thing that is broken.
                const sr0 = sc.getBoundingClientRect()
                const bt0 = tall.getBoundingClientRect().top
                sc.scrollTop += bt0 - sr0.top + head.offsetHeight + 80
                await new Promise((r) => setTimeout(r, 80))
                const sr = sc.getBoundingClientRect()
                // The header belongs flush at the top of the transcript. A sticky offset is
                // measured from the scrollport's content edge, so a scroller with a top gutter
                // would otherwise park it a gutter's height too low, with a band of scrolling
                // code above the buttons - the same complaint, 24px smaller. `.code-head`
                // subtracts the gutter to account for this, and the number below is what that
                // buys: flush, not inset.
                const stickAt = sr.top
                const mid = {
                  head: head.getBoundingClientRect(),
                  copy: copy.getBoundingClientRect(),
                  blk: tall.getBoundingClientRect(),
                }
                if (mid.blk.top > stickAt)
                  verdict.push(
                    `the block never scrolled past the sticky line (its top is still ${Math.round(mid.blk.top - stickAt)}px below it), so the check cannot say anything about its header`,
                  )
                else if (Math.abs(mid.head.top - stickAt) > 2)
                  verdict.push(
                    `80 lines into a long block the header sits ${Math.round(mid.head.top - stickAt)}px from the line it should stick at rather than at it`,
                  )
                else if (mid.copy.top < sr.top - 1 || mid.copy.bottom > sr.bottom + 1)
                  verdict.push('the copy button is outside the window while the block is being read')
                else if (mid.head.bottom > mid.blk.bottom + 1)
                  verdict.push('the header left its own block')
                // 2. Past the end of the block: the header has gone with it.
                sc.scrollTop += tall.offsetHeight
                await new Promise((r) => setTimeout(r, 80))
                const after = {
                  head: head.getBoundingClientRect(),
                  blk: tall.getBoundingClientRect(),
                }
                if (after.blk.bottom > stickAt + 2)
                  verdict.push(
                    `the transcript would not scroll far enough to get past the end of the block (its bottom is still ${Math.round(after.blk.bottom - stickAt)}px below the sticky line)`,
                  )
                else if (after.head.bottom > stickAt + 2)
                  verdict.push(
                    `the header floated above the end of its block by ${Math.round(after.head.bottom - after.blk.bottom)}px instead of ending with it`,
                  )
                sc.scrollTop = prevTop
                tall.remove()
                filler.remove()
                if (verdict.length) fail(verdict.join('; '))
                else
                  pageLog(
                    'code header sticks for the length of its block and leaves with it (copy button reachable 80 lines in)',
                  )
              }
              sc.scrollTop = prevTop
              tall.remove()
              filler.remove()
            }
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

      // --- a fenced block arrives as a block, while it is still being written ----
      // Reported as: the model writes an HTML page, and while it writes it the reader watches
      // backticks and raw markup; the block's buttons show up afterwards and have words on them.
      // What should happen, in the words of the report: "on the first detection of the block text,
      // just render the block already, so that while the LLM types, the block is already rendered
      // (just disable the buttons until it's closed properly with ```), while generating keep it
      // expanded, when it closes collapse it, with a way to toggle - and the buttons should be
      // icon-based, which applies to all blocks, not just code, html or svg".
      //
      // Driven through a real streamed turn rather than by feeding the transcript directly. That is
      // not a stylistic choice: an earlier version of this check called apply() by hand and passed
      // its own premise while the shipped path rendered nothing, which is exactly the kind of green
      // that hides the bug.
      if (want('live-fence', 'a fenced block arrives as a block, while it is still being written')) {
        const verdict: string[] = []
        const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms))
        // Scoped to this turn. The document still holds earlier cases' blocks - including a 1-line
        // html fence and a plain fence whose kind resolves to md - and a document-wide query handed
        // this check the wrong block, which is how it managed to report "the model gave us 1 line"
        // for a fifteen-line fixture that the mock had in fact served.
        const lastEl = (sel: string) =>
          ([...document.querySelectorAll(sel)].pop() ?? null) as HTMLElement | null
        transcript?.clear()
        await invoke('set_reasoning', { level: 'off', persist: true })
        // Fifteen lines, deliberately over the fold threshold: the fold is half of what is being
        // claimed, and a fixture under the threshold cannot test it.
        draft =
          'Write an html fence with fifteen lines inside it and no prose. The fifteen lines are: ' +
          '<div class="wrap"> then thirteen lines that read <p>line 1</p> through <p>line 13</p>, ' +
          'then a final </div>.'
        const turnsBefore = (await invoke<HostState>('session_state')).turns
        await send()

        // 1. while the model is still inside the fence: the block is on screen as a block. Polled
        //    on a timer because the harness already runs on one; the claim is that this state
        //    happens at all, and for how long it lasts is not part of it.
        let saw: HTMLElement | null = null
        let firstMeta = ''
        let liveAbsentDiag = ''
        for (let t = 0; t < 1200 && !saw; t += 60) {
          await sleep(60)
          const s = lastEl('.codeblk.streaming')
          if (s) {
            saw = s
            firstMeta = s.querySelector('.code-meta')?.textContent ?? ''
          }
        }
        if (saw) {
          const acts = [...saw.querySelectorAll<HTMLElement>('.code-act')]
          if (!acts.length) verdict.push('the live block has no actions at all')
          const notWaiting = acts.filter((b) => !b.matches(':disabled'))
          if (notWaiting.length)
            verdict.push(`actions worked before the fence closed: ${notWaiting.map((b) => b.dataset.act).join(',')}`)
          const wordy = acts.filter((b) => b.textContent?.trim() !== '' || !b.querySelector('svg.act-ico'))
          if (wordy.length)
            verdict.push(
              `actions carry words instead of glyphs: ${wordy.map((b) => `${b.dataset.act}="${b.textContent?.trim()}"`).join(' ')}`,
            )
          if (acts.some((b) => !b.title || !b.getAttribute('aria-label')))
            verdict.push('an icon action has neither a title nor an aria-label')
          if (saw.querySelector('pre h1') || saw.querySelector('pre code h1'))
            verdict.push('HTML inside the fence ran as live markup')
          if ((saw.querySelector('pre code')?.querySelectorAll('span') ?? []).length)
            verdict.push('the code body is being animated word by word')
          if (!/\d+ lines/.test(firstMeta))
            verdict.push(`the live block does not say how long it is: "${firstMeta}"`)
        } else {
          // Held, not pushed: whether this is a bug or an uncomposited window is only decidable
          // once the turn has ended and *this* turn's frame count is known.
          liveAbsentDiag =
            'no block appeared while the fence was open; ' +
            `codeblks=${document.querySelectorAll('.codeblk').length} ` +
            `streamingEls=${document.querySelectorAll('.streaming').length} ` +
            `liveParas=${document.querySelectorAll('.live-para').length} ` +
            `answerTail=${JSON.stringify((document.querySelector('.blk-answer')?.textContent ?? '').slice(-120))}`
        }
        if (!(await waitTurnDone(turnsBefore))) fail('the live-block turn never finished')
        await sleep(400)
        if (liveAbsentDiag) {
          // Whether a live fence was observable depends on the page being composited. With no frames
          // painted during the turn every delta lands in one coalesced flush, and the fence opens
          // and closes between two paints. That is the window, not the feature, so it is reported
          // as a blind spot rather than a failure - and the settled half still has to hold, because
          // that half does not care when the pixels arrived.
          if (lastTurnFrames === 0)
            pageLog(
              'selftest note: the live-block claim was not observable on this run - the turn painted no '
                + 'animation frames, so the fence opened and closed between two paints. The settled half '
                + 'of the check still ran.',
            )
          else verdict.push(`${liveAbsentDiag} and the turn painted ${lastTurnFrames} frames`)
        }

        // 2. the fence closed: the block is still there, its actions now work, and a block this
        //    long arrives folded, because what it costs the reader is the scroll past its source.
        const done = lastEl('.codeblk[data-lang="html"]')
        if (!done) verdict.push('the block vanished when the fence closed')
        else {
          if (done.classList.contains('streaming'))
            verdict.push('the block stayed marked streaming after the turn ended')
          const acts = [...done.querySelectorAll<HTMLElement>('.code-act')]
          const stuck = acts.filter((b) => b.matches(':disabled'))
          if (stuck.length)
            verdict.push(`actions stayed disabled after the fence closed: ${stuck.map((b) => b.dataset.act).join(',')}`)
          const lines = Number(done.dataset.lines ?? '0')
          if (lines < 12)
            verdict.push(`the model gave us ${lines} lines; the premise needs a block over the fold threshold`)
          else if (!done.classList.contains('folded'))
            verdict.push(`a ${lines}-line html block arrived unfolded: the reader scrolls past every line of it`)
          const eye = done.querySelector('.code-act[data-act="render"]') as HTMLElement | null
          if (!eye) verdict.push('a renderable block has no render action')
          else {
            eye.click()
            const frame = done.querySelector('.pv-inline-frame') as HTMLIFrameElement | null
            if (!frame) verdict.push('the render action showed nothing')
            else {
              if (frame.getAttribute('sandbox') !== 'allow-scripts')
                verdict.push('the inline render is not sandboxed the way the panel is')
              if (!frame.srcdoc.includes('line 1')) verdict.push('the inline render is not the block content')
              if (eye.getAttribute('aria-pressed') !== 'true') verdict.push('the render action does not say it is on')
            }
            eye.click()
            if (done.querySelector('.pv-inline-frame')) verdict.push('the render action will not turn itself off')
          }
          const fold = done.querySelector('.code-act[data-act="fold"]') as HTMLElement | null
          if (!fold) verdict.push('the block has no fold action')
          else {
            if (getComputedStyle(done.querySelector('pre')!).display === 'none')
              verdict.push('a folded block hides its source twice over')
            fold.click()
            if (!done.classList.contains('folded')) verdict.push('the fold action unfolded an already folded block')
            if (getComputedStyle(done.querySelector('pre')!).display !== 'none')
              verdict.push('a folded block still shows its source')
            fold.click()
            if (done.classList.contains('folded')) verdict.push('the fold will not open back up')
            if (fold.getAttribute('aria-expanded') !== 'true') verdict.push('the fold action does not say it is open')
          }
          // The block must answer what it is and how big it is while folded, or collapsing it is
          // just hiding information.
          if (done.classList.contains('folded')) {
            const meta = done.querySelector('.code-meta')?.textContent ?? ''
            if (!/\d+ lines/.test(meta)) verdict.push(`folded and not saying how long it is: "${meta}"`)
          }
          // Line numbers. They are only worth having if they point at the right lines, stay out of
          // the clipboard, and are not read aloud as "one two three" in front of the code.
          const gutter = done.querySelector('pre .ln') as HTMLElement | null
          const codeEl = done.querySelector('pre code') as HTMLElement | null
          if (!gutter) verdict.push('the code block has no line-number gutter')
          else if (!codeEl) verdict.push('the block has a gutter and no code element to line it up with')
          else {
            if (gutter.getAttribute('aria-hidden') !== 'true')
              verdict.push('the gutter is not hidden from a screen reader, which reads its digits before the code')
            if (codeEl.contains(gutter))
              verdict.push('the gutter is inside the code element, so copying the block copies its numbers too')
            const nums = (gutter.textContent ?? '').split('\n').filter(Boolean).map(Number)
            if (nums.length !== lines)
              verdict.push(`the gutter numbers ${nums.length} lines while the block holds ${lines}`)
            else if (nums[0] !== 1 || nums[nums.length - 1] !== lines)
              verdict.push(`the gutter runs ${nums[0]}..${nums[nums.length - 1]} across a ${lines}-line block`)
            const gs = getComputedStyle(gutter)
            const cs = getComputedStyle(codeEl)
            if (Math.abs(parseFloat(gs.lineHeight) - parseFloat(cs.lineHeight)) > 0.6)
              verdict.push(
                `the gutter's leading is ${gs.lineHeight} against the code's ${cs.lineHeight}: the numbers drift off their lines`,
              )
            // WebKitGTK answers for the prefixed spelling, so read both. A check that reads only the
            // unprefixed one fails on a rule that is applied, which is how this one first reported.
            const selMode =
              (gs.userSelect && gs.userSelect !== 'auto' ? gs.userSelect : gs.getPropertyValue('-webkit-user-select')) || ''
            if (selMode.trim() !== 'none')
              verdict.push(
                `the gutter is selectable (user-select computes to "${selMode || 'nothing'}"), so selecting the code drags the numbers into the clipboard`,
              )
            const lineTop = (el: HTMLElement) => {
              const t = el.firstChild
              if (!(t instanceof Text) || !t.length) return NaN
              const r = document.createRange()
              r.setStart(t, 0)
              r.setEnd(t, 1)
              return r.getBoundingClientRect().top
            }
            const gt = lineTop(gutter)
            const ct = lineTop(codeEl)
            if (Number.isFinite(gt) && Number.isFinite(ct) && Math.abs(gt - ct) > 1)
              verdict.push(`the first number sits ${Math.round(gt - ct)}px off the first line of code`)
          }
          // The copy action, pressed for real. It used to write the word "copied" into a 24px icon
          // box - clipped to about "pie" - and then restore the button's old textContent, which for
          // an icon button is the empty string: the glyph went away and the invisible button stayed
          // clickable. So: no words at any point, a glyph before and after, and the same glyph at the
          // end.
          const cp = done.querySelector('.code-act[data-act="copy"]') as HTMLButtonElement | null
          if (!cp) verdict.push('the block has no copy action')
          else {
            const glyph = cp.innerHTML
            cp.click()
            await sleep(90)
            if (cp.textContent.trim())
              verdict.push(`the copy action became the word "${cp.textContent.trim()}" instead of an icon`)
            if (!cp.querySelector('svg'))
              verdict.push('pressing copy took the button glyph and gave nothing back')
            await sleep(1600)
            if (!cp.querySelector('svg'))
              verdict.push('the copy button was left without a glyph after its confirmation')
            else if (cp.innerHTML !== glyph)
              verdict.push('the copy button came back as a different glyph than the one it started with')
            const cpr = cp.getBoundingClientRect()
            if (cpr.width < 20 || cpr.height < 18)
              verdict.push(`the copy button is ${Math.round(cpr.width)}x${Math.round(cpr.height)}px, too small to aim at`)
          }
        }

        // 3. a ```md fence is the same feature with a different rendered form: prose, not a page.
        transcript?.clear()
        draft =
          'Write a markdown fence with a heading, a bold phrase and a long paragraph, and no prose ' +
          'outside the fence.'
        const turnsMd = (await invoke<HostState>('session_state')).turns
        await send()
        if (!(await waitTurnDone(turnsMd))) fail('the markdown fence turn never finished')
        await sleep(400)
        const mdblk = lastEl('.codeblk[data-kind="md"]')
        if (!mdblk)
          verdict.push(
            'a ```md fence did not render as a block; ' +
              `codeblks=${document.querySelectorAll('.codeblk').length} ` +
              `streaming=${document.querySelectorAll('.codeblk.streaming').length} ` +
              `answerTail=${JSON.stringify((document.querySelector('.blk-answer')?.textContent ?? '').slice(-160))}`,
          )
        else if (!(mdblk.querySelector('pre code')?.textContent ?? '').includes('Field notes'))
          verdict.push('the md block under test is not this turn\'s: its source has no heading in it')
        else {
          const eye = mdblk.querySelector('.code-act[data-act="render"]') as HTMLElement | null
          if (!eye) verdict.push('a ```md block has no render action')
          else {
            eye.click()
            const r = mdblk.querySelector('.md-render')
            if (!r) verdict.push('the ```md render action showed nothing')
            else {
              if (!r.querySelector('h3')) verdict.push('the ```md render kept the literal ### instead of a heading')
              if (!r.querySelector('strong')) verdict.push('the ```md render kept the literal ** markers')
            }
            eye.click()
            if (mdblk.querySelector('.md-render')) verdict.push('the ```md render will not turn itself off')
          }
        }
        transcript?.clear()
        if (verdict.length) fail(`live blocks: ${verdict.join('; ')}`)
        else
          pageLog(
            'live blocks ok: a fence renders as a block while the model is still inside it, its icon actions wait for the closing fence, ' +
              'a long renderable block arrives folded with its size in the head, and the eye and the fold both toggle',
          )
      }

      // Remote access, driven through the real commands. The sidebar row's gateway icon now
      // claims the gateway is on, on a given port, with N devices attached (in its title and dot);
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
          // The card belongs to the run it is waiting inside, not to the transcript as a
          // loose strip: the report is that an open question "clutters the conversation".
          // And it has to be visible while it waits, which is the one thing that overrides
          // the group's new default of being folded.
          const qrun = card.closest('.blk-run') as HTMLElement | null
          if (!qrun) fail('a question card sits outside the grouped run it is waiting in')
          else {
            if (!qrun.querySelector('.run-body > .blk-ask'))
              fail('the question card is not in the run body')
            if (!qrun.classList.contains('open'))
              fail('the run holding an unanswered question is folded, so the question nobody has answered is invisible')
            if (qrun.dataset.ask !== '1')
              fail(`the run holding a pending question says it has ${qrun.dataset.ask} of them`)
            if (!(qrun.querySelector('.run-label')?.textContent ?? '').includes('waiting for you'))
              fail('a run waiting on a person does not say so in its header')
          }
          // A paused turn is not a failed one. The card used to carry a countdown, and when
          // it ran out the model was told nothing had been heard - which the report calls
          // out as a timeout where there should be a pause.
          const ctext = card.textContent ?? ''
          if (/\b(60|120|180)\b|left\b|timed out|no answer/i.test(ctext))
            fail(`the question card still talks about a clock: ${ctext.replace(/\s+/g, ' ').slice(0, 160)}`)
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
          // Answered: the run goes back to being folded, because the only thing holding it
          // open was the question. A card that keeps the group open after being answered is
          // the clutter the report was about.
          const qrun2 = card.closest('.blk-run') as HTMLElement | null
          if (qrun2 && qrun2.dataset.ask !== '0')
            fail(`an answered question still holds its run open (dataset.ask = ${qrun2.dataset.ask})`)
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

        // Which sentence a closed card ends with is the part the reader can act on. A turn
        // they stopped themselves is not a turn the model gave up on, and the line the
        // question leaves in the transcript is the only place that fact can be read later.
        {
          const reasons: [string, RegExp, RegExp | null][] = [
            ['turnStopped', /turn was stopped/, /was told it got none/],
            ['timedOut', /clock ran out/, null],
            ['windowGone', /window holding this card closed/, /was told it got none/],
          ]
          let n = 900_010
          for (const [why, want, forbidden] of reasons) {
            transcript?.showQuestion(
              { id: n, question: `Which month (${why})?`, options: [{ label: 'January' }], allow_custom: false, multi: false },
              () => fail('a closed question must not send an answer'),
            )
            const c = document.querySelector('.blk-ask:last-of-type')
            if (!c) fail(`the ${why} question card did not render`)
            else {
              transcript?.expireQuestion(n, why as 'turnStopped' | 'timedOut' | 'windowGone')
              const line = c.querySelector('.ask-expired')?.textContent ?? ''
              if (!want.test(line))
                fail(`a question closed by ${JSON.stringify(why)} says ${JSON.stringify(line.trim())}`)
              else if (forbidden && forbidden.test(line))
                fail(`a question closed by ${JSON.stringify(why)} claims something that did not happen: ${JSON.stringify(line.trim())}`)
              c.remove()
            }
            n += 1
          }
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
          const asked = [...document.querySelectorAll('.blk-asked')].filter(
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
      if (want('mode-tab', 'the mode tab states a direction without pretending to work')) {
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
      if (want('file-reload', 'the file the model wrote is a chip, live and after a reload')) {
        const essayPath = 'notes/selftest-espresso.txt'
        await useSession(target.id)
        const turnsBefore = await turnCount()
        await invoke('set_reasoning', { level: 'off', persist: true })
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
        await invoke('set_reasoning', { level: 'medium', persist: true })
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
          else if (maxRunning < 3 && lastTurnFrames === 0)
            // Honest skip, and the reason: nothing painted during the turn, so there was no
            // moment between the stream and the settle in which to look at the DOM. The final
            // structure above is still asserted; only the mid-stream half is unobservable here.
            pageLog(
              `selftest note: skipped the mid-stream half of the bullet check; this window delivered ` +
              `0 paints during the turn (${liCounts.length} samples all saw ${maxRunning} li, and the list is ` +
              `whole at settle), so live structure cannot be observed in a window that is not being composited`,
            )
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

        // (a6) the model totals a column instead of writing its own parser
        //
        // The transcript this pins is a real one: after `sheet_read` had shown it
        // the header row and 15 rows of a 1000-row workbook, the model could not
        // total a column, so it wrote a 111-line Python script that unzipped the
        // package and parsed sharedStrings.xml by hand - after two failed calls
        // assuming openpyxl existed and that /tmp survived between commands. The
        // scenario's `require` strings are the engine's own result grid, so a tool
        // that ran but returned nothing useful fails the run instead.
        {
          const before = await turnCount()
          const ws = (hostState?.workspace ?? '').replace(/\/+$/, '')
          const file = `${ws}/selftest-query.csv`
          if (!ws) fail('the self-test host has no workspace, so the sheet-query fixture path is unknown')
          await invoke('start_turn', {
            text:
              `Selftest sheet-query: put a four-row region table into ${file} (Region, Units, Price) ` +
              'and then total Units by Region with one query and tell me the biggest region.',
            session: target.id,
          }).catch((e) => pageLog(`selftest note: sheet-query turn failed: ${e}`))
          const done = await waitTurnDone(before, 60000)
          const cards = [...document.querySelectorAll('.blk-tool[data-tool="sheet_query"]')]
          const card = cards[cards.length - 1]
          const out = card?.querySelector('.tool-out')?.textContent ?? ''
          const head = card?.querySelector('.tool-name')?.textContent ?? ''
          const answers = [...document.querySelectorAll('.blk-answer .blk-body')].map(
            (b) => b.textContent ?? '',
          )
          if (!done) fail('the scripted sheet-query turn never finished')
          else if (!card) fail('no tool card for sheet_query: the model never reached the query tool')
          else if (!out.includes('West\t26'))
            fail(
              'the query result grid is not what a total looks like: ' +
                JSON.stringify(out.slice(0, 220)),
            )
          else if (!out.includes('rownum | region | units | price'))
            fail(`the result does not name the table's columns: ${JSON.stringify(out.slice(0, 220))}`)
          else if (!answers.some((t) => t.includes('West is the biggest')))
            fail(
              `the grid reached the card but not the answer: ${answers.length} answer block(s) - ` +
                'the loop stopped before its last step',
            )
          else
            pageLog(
              `sheet_query ok ("${head.trim()}", the card shows West/26 from the engine's own result grid: ` +
                'a column totalled without the model writing a parser)',
            )
        }

        // (a7) a header below a title block is still a header
        //
        // The cash-flow workbook the user actually handed the model opens with
        // "BURGOS ELEMENTARY SCHOOL" in row 1 and the treasurer's name in row 2;
        // the header is row 3. Naming columns from row 1 made `sheet_query` return
        // `burgos_elementary_school | c2 | c3`, i.e. unusable, which is how a model
        // ends up unzipping an .xlsx in a shell. So the detector scores rows, and
        // this check makes the tool say which row it used.
        {
          const before = await turnCount()
          const ws = (hostState?.workspace ?? '').replace(/\/+$/, '')
          const file = `${ws}/selftest-titled.csv`
          if (!ws) fail('the self-test host has no workspace, so the titled-sheet fixture path is unknown')
          await invoke('start_turn', {
            text:
              `Selftest titled-sheet: write ${file} with a school name on the first line, a ` +
              'treasurer on the second, then a DATE,DESCRIPTION,INCOME header and two data rows, ' +
              'then total INCOME by description in one query.',
            session: target.id,
          }).catch((e) => pageLog(`selftest note: titled-sheet turn failed: ${e}`))
          const done = await waitTurnDone(before, 60000)
          const cards = [...document.querySelectorAll('.blk-tool[data-tool="sheet_query"]')]
          const card = cards[cards.length - 1]
          const out = card?.querySelector('.tool-out')?.textContent ?? ''
          const answers = [...document.querySelectorAll('.blk-answer .blk-body')].map(
            (b) => b.textContent ?? '',
          )
          if (!done) fail('the scripted titled-sheet turn never finished')
          else if (!card) fail('no tool card for sheet_query on the titled sheet')
          else if (!out.includes('# header: sheet row 3'))
            fail(
              'the tool did not say which row named the columns (a title block is not a header): ' +
                JSON.stringify(out.slice(0, 260)),
            )
          else if (!out.includes('Books\t250'))
            fail(`the titled sheet did not aggregate: ${JSON.stringify(out.slice(0, 260))}`)
          else if (!answers.some((t) => t.includes('header was row 3')))
            fail('the header note reached the card but not the answer')
          else
            pageLog(
              'titled header ok (header found on sheet row 3, the two title rows above it are not ' +
                'records, and the card says so)',
            )
        }

        // (a8) a turn is not capped at fourteen tool calls
        //
        // Fourteen was a work budget wearing a guard's clothes: the cash-flow
        // session spent all of them reading eight sheets, then the engine forced a
        // summary of work it had not finished. The cap is now `max_tool_iters`
        // (0 = none, the default), and this scripted turn makes seventeen calls -
        // it cannot complete at all unless the cap is gone.
        {
          const before = await turnCount()
          const ws = (hostState?.workspace ?? '').replace(/\/+$/, '')
          const file = `${ws}/selftest-long.csv`
          if (!ws) fail('the self-test host has no workspace, so the long-turn fixture path is unknown')
          await invoke('start_turn', {
            text:
              `Selftest long-turn: put three lines into ${file}, then read it back seventeen ` +
              'times and tell me the last line.',
            session: target.id,
          }).catch((e) => pageLog(`selftest note: long-turn turn failed: ${e}`))
          const done = await waitTurnDone(before, 120000)
          const reads = document.querySelectorAll('.blk-tool[data-tool="read_file"]').length
          const notes = [...noteLog]
          const answers = [...document.querySelectorAll('.blk-answer .blk-body')].map(
            (b) => b.textContent ?? '',
          )
          if (!done) fail('a 17-call turn never finished: the tool loop is capped somewhere')
          else if (reads < 16)
            fail(`only ${reads} read_file calls reached the transcript: the loop stopped early`)
          else if (!answers.some((t) => t.includes('never capped at fourteen')))
            fail(`seventeen calls ran but the answer never arrived (${answers.length} answer block(s))`)
          else if (!notes.some((t) => t.includes('no cap set')))
            fail('an uncapped turn ran 20+ calls without ever saying it was uncapped')
          else
            pageLog(
              `long turn ok (${reads} tool calls in one turn, the answer arrived, and the turn ` +
                'said once that it was uncapped)',
            )
        }

        // (a9) a turn belongs to the conversation the engine says it belongs to
        //
        // Two reports, one root: `turnStart` never said which session a turn
        // wrote into, so the UI guessed from whatever was on screen. Thinking
        // from session A appeared inside session B; and coming back to A showed
        // nothing, because the buffer that exists to replay a partial turn held
        // one entry per delta, hit its cap, and kept the *oldest* 4000 - so the
        // part that was lost was the part still being written.
        {
          const sessA = await invoke<number>('new_session', { title: 'selftest ownership A' })
          const sessB = await invoke<number>('new_session', { title: 'selftest ownership B' })
          await useSession(sessA)
          let wireOwner: number | null = null
          let evBuf = 0
          let bufEntries = 0
          let bufEvents = 0
          const unTap = beginEventTap((e) => {
            if (e.kind === 'turnStart') wireOwner = e.d.session_id ?? null
            if (e.kind === 'turnEnd') return
            // Count only what the buffer is for: the live gauge is dropped on
            // purpose, so it must not count as something the buffer failed to keep.
            if (e.kind !== 'turnStats') evBuf++
            // Keep the fullest sample. A ratio measured two deltas into a turn
            // says nothing; measured at the end of one it says whether the tail
            // would have survived a longer turn.
            if (liveBuf.length > 0 && evBuf > bufEvents) {
              bufEvents = evBuf
              bufEntries = liveBuf.length
            }
          })
          const turnsBeforeOwn = await turnCount()
          draft =
            'Selftest live ownership: think in lines about plumlines, then list five seaglass items.'
          await send()
          const sawOwn = await waitFor('the ownership turn to start', () => busy && turnSessionId === sessA, 90_000)
          if (!sawOwn) {
            fail('the ownership turn never started')
          } else if (wireOwner !== sessA) {
            fail(
              `turnStart named session ${wireOwner}, not ${sessA}: the engine does not say which ` +
                'conversation a running turn writes into, so every client has to guess',
            )
          } else {
            await openSession(sessB)
            const whileAway: string[] = []
            for (let i = 0; i < 10; i++) {
              whileAway.push(rootEl?.innerText ?? '')
              await new Promise((r) => setTimeout(r, 60))
            }
            const leaked = whileAway.find((t) => t.includes('plumline') || t.includes('seaglass'))
            if (leaked)
              fail(
                `session B showed another conversation's output while it was being written ` +
                    `(${whileAway.length} samples): "${leaked.slice(0, 120)}"`,
              )
            else if (turnSessionId !== sessA)
              fail(`looking at session B moved the turn: turnSessionId=${turnSessionId}`)
            else {
              await resyncView(sessA)
              const back = rootEl?.innerText ?? ''
              if (!back.includes('plumline'))
                fail(
                  `coming back to the running session lost what it had already thought: ` +
                    `"${back.slice(0, 140)}" (${bufEvents} events streamed so far)`,
                )
              else if (!(await waitTurnDone(turnsBeforeOwn, 90_000)))
                fail('the ownership turn never finished')
              else {
                const done = rootEl?.innerText ?? ''
                if (!done.includes('seaglass epsilon'))
                  fail(
                    `the turn ended but its tail is not in the transcript: "${done.slice(-160)}" - ` +
                      `the replay buffer held ${bufEntries} entries for ${bufEvents} events`,
                  )
                else if (bufEntries * 2 >= bufEvents)
                  fail(
                    `the replay buffer held ${bufEntries} entries for ${bufEvents} events: nothing ` +
                      'merged, so a long turn overflows the cap and loses the part still being written',
                  )
                else
                  pageLog(
                    `live turn ownership ok (turnStart named session ${sessA}; session B stayed clean ` +
                      `across ${whileAway.length} samples; coming back kept the thinking and the tail; ` +
                      `${bufEvents} events replayed from ${bufEntries} buffer entries)`,
                  )
              }
            }
          }
          unTap()
          if ((await invoke<HostState>('session_state')).busy) {
            await invoke('cancel_turn').catch(() => {})
            const idleBy = performance.now() + 30000
            while (performance.now() < idleBy) {
              if (!(await invoke<HostState>('session_state')).busy) break
              await new Promise((r) => setTimeout(r, 250))
            }
          }
          await invoke('delete_session', { id: sessA }).catch((e) => pageLog(`cleanup A: ${e}`))
          await invoke('delete_session', { id: sessB }).catch((e) => pageLog(`cleanup B: ${e}`))
          await useSession(target.id)
        }

        // (a10) "Load older messages" reads the conversation on screen
        //
        // The button vanished on click. It asked the engine for the session the
        // *engine* was working on, which - since browsing stopped moving the
        // host - is a different conversation, and `has_older` for that one was
        // false. So the button was told to hide itself by a question about
        // somewhere else. This fills a session past the 80-message page, starts
        // a turn in a *different* session, and then clicks.
        {
          const sessH = await invoke<number>('new_session', { title: 'selftest history fill' })
          const ws = (hostState?.workspace ?? '').replace(/\/+$/, '')
          if (!ws) fail('the self-test host has no workspace, so the history fixture path is unknown')
          // Three passes, each writing a file of its own. One pass stores ~56
          // rows, which is barely past the page limit; three make a session that
          // behaves like the one in the report - a few hundred messages, most of
          // them not in the DOM. A different file per pass matters: overwriting
          // the same path is a Workspace-risk write, and the engine's answer for
          // an overwrite is "overwrote N bytes", not "created" - the mock's
          // `require` caught the harness assuming otherwise.
          for (let pass = 0; pass < 3; pass++) {
            const file = `${ws}/selftest-fill-${pass + 1}.txt`
            const grewBefore = await invoke<any>('session_messages', { id: sessH, limit: 2000 })
            const beforeRows: number = (grewBefore.messages ?? []).length
            const turnsBeforeFill = await turnCount()
            await invoke('start_turn', {
              text: `Selftest history fill pass ${pass + 1}: write ${file} with two rows, then read it back 25 times.`,
              session: sessH,
            }).catch((e) => pageLog(`selftest note: history fill turn failed: ${e}`))
            if (!(await waitTurnDone(turnsBeforeFill, 180000))) {
              fail(`the history fill turn (pass ${pass + 1}) never finished`)
              break
            }
            const grewAfter = await invoke<any>('session_messages', { id: sessH, limit: 2000 })
            const afterRows: number = (grewAfter.messages ?? []).length
            // The premise of everything below is a long session. A turn that ended
            // in a provider error still counts as "finished" to the counter, so the
            // rows it stored are the evidence, not the turn counter.
            if (afterRows - beforeRows < 40)
              fail(
                `history fill pass ${pass + 1} stored ${afterRows - beforeRows} rows (expected ~56): ` +
                  `the turn ended early - session ${sessH} holds ${afterRows} rows total`,
              )
          }
          await useSession(sessH)

          const stored = await invoke<any>('session_messages', { id: sessH, limit: 2000 })
          const total: number = (stored.messages ?? []).length
          const btnText = 'Load older messages'
          const btn = () =>
            [...document.querySelectorAll('button')].find((b) =>
              (b.textContent ?? '').includes(btnText),
            )
          if (total <= 80)
            fail(`the fill turn stored ${total} messages; the check needs more than the 80-message page`)
          else if (!hasOlder)
            fail(`session ${sessH} holds ${total} messages and the page said has_older=false`)
          else if (!btn())
            fail('a session with older messages shows no "Load older messages" button')
          else {
            // Make the engine's session and the session on screen disagree, which
            // is the exact situation the old code got wrong.
            const sessOther = await invoke<number>('new_session', { title: 'selftest history other' })
            const turnsBeforeOther = await turnCount()
            await invoke('start_turn', {
              text: 'Selftest live ownership: think in lines about plumlines, then list five seaglass items.',
              session: sessOther,
            })
            const sawOther = await waitFor(
              'the other session to start a turn',
              () => busy && turnSessionId === sessOther,
              90_000,
            )
            await refreshState()
            const hostNow = hostState?.session_id ?? null
            if (!sawOther) fail('the second session never started a turn')
            else if (hostNow === sessH)
              fail('the check could not make the engine work on a different session, so it proves nothing')
            else {
              const beforeCount = viewMsgCount
              const beforeOldest = oldestId
              await loadOlder()
              const page = await invoke<any>('session_messages', {
                id: sessH,
                before: beforeOldest,
                limit: 80,
              })
              const got: number = (page.messages ?? []).length
              if (viewMsgCount === beforeCount)
                fail(
                  `"Load older messages" added nothing while session ${sessH} (on screen) had ` +
                    `${total - beforeCount} older rows and the engine was on ${hostNow}: it asked the ` +
                    'wrong conversation and then hid the button',
                )
              else if (got !== viewMsgCount - beforeCount)
                fail(
                  `the view grew by ${viewMsgCount - beforeCount} but the store's page for ${sessH} ` +
                    `held ${got} - the rows came from somewhere else`,
                )
              else if (hasOlder !== page.has_older)
                fail(`the button's visibility (${hasOlder}) disagrees with the store (${page.has_older})`)
              else if (hasOlder && !btn())
                fail('the store says older rows remain but the button is gone')
              else if (!hasOlder && btn())
                fail('the button stayed after the history was exhausted')
              else
                pageLog(
                  `load history ok (${beforeCount} -> ${viewMsgCount} rows of session ${sessH} while ` +
                    `the engine worked on ${hostNow}; button ${hasOlder ? 'still offered' : 'retired at the end of the history'})`,
                )
            }
            if ((await invoke<HostState>('session_state')).busy) {
              await invoke('cancel_turn').catch(() => {})
              const idleBy = performance.now() + 30000
              while (performance.now() < idleBy) {
                if (!(await invoke<HostState>('session_state')).busy) break
                await new Promise((r) => setTimeout(r, 250))
              }
            }
            await invoke('delete_session', { id: sessOther }).catch((e) => pageLog(`cleanup other: ${e}`))

            // (a11) the same turn with a full transcript on screen
            //
            // "Native UI performance drops badly when history grows and during
            // inference; the browser build is smooth." The browser comparison is
            // the clue: same page, same JS, different engine. So the measurement
            // has to be the native window holding a full page of history while a
            // turn streams into it, judged against the same page idling - an
            // absolute 60Hz number on a box serving a 50 GB model measures the
            // box, not the app.
            {
              // Pull the rest of the history in first: the complaint is about a
              // transcript that has grown, and a page that only holds the last
              // 80 rows does not measure that.
              let pages = 0
              while (hasOlder && pages < 6) {
                await loadOlder()
                pages++
              }
              const nodes = document.querySelectorAll('.transcript .blk').length
              // Verify the premise before measuring it. The complaint is about a
              // transcript that has grown, and the only thing that makes this check
              // different from the plain cadence check below is that the grown
              // history is actually mounted. Count the characters, not the rows: a
              // page of empty assistant rows is cheap to paint whatever the row
              // count says, and this fixture's bulk lives inside the tool cards.
              const mountedMsgs = viewMsgCount
              const chars = (document.querySelector('.transcript')?.textContent ?? '').length
              if (mountedMsgs < 150 || chars < 100_000)
                fail(
                  `the big-transcript cadence check measured ${mountedMsgs} messages / ${nodes} blocks / ` +
                    `${chars} characters on screen: the fill session holds ~168 rows and most of them are ` +
                    `200-line tool results, so the older pages are not mounted and this check would be ` +
                    `measuring the same small transcript the plain cadence check already measures`,
                )
              await invoke('set_reasoning', { level: 'off', persist: true })
              const cadBefore = await turnCount()
              const charsBefore = chars
              const blocksBefore = nodes
              draft =
                'Selftest cadence heavy: write the long roast-curve note again, in the chat only, ' +
                'with every table and heading kept. Use no tools and create no files.'
              await send()
              const stopBig = startFrameCadence()
              const cadDone = await waitTurnDone(cadBefore, 120000)
              const big = stopBig()
              // The measurement is only about a big streaming answer if a big streaming
              // answer actually arrived, as structure. Four short paragraphs against a
              // grown transcript is the plain cadence check with extra steps.
              const charsAfter = (document.querySelector('.transcript')?.textContent ?? '').length
              const blocksAfter = document.querySelectorAll('.transcript .blk').length
              const byClass = (want: string) => document.querySelectorAll(`.transcript ${want}`).length
              const tables = document.querySelectorAll('.transcript table').length
              const grew = charsAfter - charsBefore
              // The premise is about the turn that just streamed, so it is measured on
              // that turn. A count over the whole mounted transcript can be flat while a
              // big answer streams, because the older pages around it are being pulled and
              // repainted at the same time - and a premise that fails for a reason off in
              // the fixture is a check that will be argued about instead of believed.
              const turnEls = [...document.querySelectorAll('.transcript .turn')] as HTMLElement[]
              const lastTurn = turnEls[turnEls.length - 1]
              const liveAnswer = lastTurn?.querySelector('.blk-answer .blk-body')
              const liveChars = (liveAnswer?.textContent ?? '').length
              const liveTables = liveAnswer?.querySelectorAll('table').length ?? 0
              if (cadDone && viewMsgCount < mountedMsgs)
                fail(
                  `a turn started in ${sessH}, which was already on screen, and the view fell from ` +
                    `${mountedMsgs} mounted rows to ${viewMsgCount}: the repaint that a session switch ` +
                    `triggers handed back the default page instead of the window the reader had loaded. ` +
                    `This is "Load history disappears" - the rows arrive, and then a repaint elsewhere undoes them.`,
                )
              if (cadDone && (grew < 4000 || liveChars < 4000 || liveTables < 2))
                fail(
                  `the big-transcript turn added ${grew} characters to the transcript and ${liveChars} of them, ` +
                    `with ${liveTables} rendered table(s), in the answer that just streamed (mounted transcript: ` +
                    `${tables} table(s), ${byClass('.blk-answer')} answers, ${byClass('.blk-run')} runs, ` +
                    `${byClass('.blk-tool')} tool entries, ${blocksAfter - blocksBefore} net blocks): a real answer ` +
                    `streams thousands of deltas of headings, lists and tables, and the cadence check has to be ` +
                    `measured against that`,
                )
              await invoke('set_reasoning', { level: 'medium', persist: true })
              let idleBig = await idleFrameSample(900)
              // An idle sample whose median is below the vsync floor is not an idle baseline: it is
              // a burst of rAF callbacks sharing one paint, which is what a page that is not being
              // composited reports. Dividing by it produced "x Infinity" and a failure that blamed
              // the history for the window manager, so take it again, and if it is still unusable
              // say so and keep only the guards that do not need it.
              if (idleBig.frames < 8 || idleBig.median < 4) {
                pageLog(
                  `selftest note: the idle cadence baseline was unusable (${idleBig.frames} frames, median ` +
                    `${idleBig.median.toFixed(1)}ms) - the page was not being painted steadily; retaking it`,
                )
                await presentWindow('the idle baseline')
                idleBig = await idleFrameSample(900)
              }
              const idleUsable = idleBig.frames >= 8 && idleBig.median >= 4
              pageLog(
                `big transcript cadence: ${nodes} blocks / ${mountedMsgs} messages / ${chars} characters mounted (${pages} older page(s) pulled in); ` +
                  `${big.frames} frames over ${big.ms}ms while a turn streamed, median ${big.median.toFixed(1)}ms, ` +
                  `p95 ${big.p95.toFixed(1)}ms, worst ${big.worst.toFixed(1)}ms, ` +
                  `page long tasks ${big.longN} totalling ${Math.round(big.longMs)}ms; ` +
                  `the stream itself added ${grew} characters / ${blocksAfter - blocksBefore} blocks / ${tables} tables; ` +
                  `idle baseline ${idleBig.frames} frames, median ${idleBig.median.toFixed(1)}ms ` +
                  `(stream/idle x${idleUsable ? (big.median / idleBig.median).toFixed(2) : 'not measured'})`,
              )
              if (!cadDone) pageLog('selftest note: the big-transcript cadence turn did not finish')
              // Two guards, because one number answers two different questions. The
              // panel here idles at ~9 ms (120 Hz) and composites a streaming turn at
              // ~17 ms: exactly 2x idle, which is frame pacing - the compositor
              // painting every other vsync - not the app getting slower. A 1.6x ratio
              // would have failed on that. So the relative guard is loose enough to
              // tolerate pacing, and the absolute one states the number the report
              // actually used: under 30 fps with a grown transcript on screen.
              else if (big.median > 33.4)
                fail(
                  `a grown transcript streams at ${big.median.toFixed(1)}ms/frame (` +
                    `${(1000 / big.median).toFixed(0)} fps): the report says the native window drops well ` +
                    `under 30 fps as history grows, and ${nodes} blocks / ${chars} characters is the size that has to hold`,
                )
              else if (idleUsable && big.median > idleBig.median * 2.2 + 3)
                fail(
                  `a full transcript makes the turn ${big.median.toFixed(1)}ms/frame against ` +
                    `${idleBig.median.toFixed(1)}ms idle on the same page (x${(big.median / idleBig.median).toFixed(1)}): ` +
                    `the cost is in the history, ${nodes} blocks were mounted`,
                )
              else if (idleUsable && big.worst > Math.max(250, idleBig.worst * 2))
                fail(
                  `a full transcript produced a ${big.worst.toFixed(0)}ms hitch while streaming ` +
                    `(idle worst ${idleBig.worst.toFixed(0)}ms)`,
                )
            }
          }
          // (a11b) what scrolling through a grown transcript costs
          //
          // Every cadence number so far is taken while the app writes the scroll position. What a
          // reader does is wheel. This session cannot fake that: on a Wayland desktop an external
          // client injecting input is a remote-desktop request, and a test run must not be the reason
          // that dialog appears. A synthetic WheelEvent inside the page is no substitute either - an
          // untrusted wheel event scrolls nothing, so it would measure a handler that never runs. The
          // part of the cost that is identical on both paths is what this measures: rows coming into
          // and out of the viewport, and the app's reaction to a position it did not write.
          {
            const sc2 = document.querySelector('.scroller') as HTMLElement | null
            if (!sc2) {
              pageLog('selftest note: no scroller on the page; the grown-transcript scroll probe did not run')
            } else {
              const mounted = document.querySelectorAll('.blk-answer, .blk-run, .blk-user').length
              const max = sc2.scrollHeight - sc2.clientHeight
              const steps = 14
              const step = Math.max(80, Math.round(max / steps))
              const cad = startFrameCadence()
              let refused = 0
              let worstMiss = 0
              sc2.scrollTop = max
              await new Promise((r) => setTimeout(r, 150))
              for (let i = 0; i < steps; i++) {
                const from = sc2.scrollTop
                const want = Math.max(0, from - step)
                sc2.scrollTop = want
                await new Promise((r) => setTimeout(r, 42))
                const miss = Math.abs(sc2.scrollTop - want)
                if (miss > 8) {
                  refused++
                  worstMiss = Math.max(worstMiss, miss)
                }
              }
              const r = cad()
              pageLog(
                `grown-transcript scroll: ${steps} wheel-sized steps of ${step}px over ${r.ms}ms on ${mounted} blocks (scrollHeight ${Math.round(sc2.scrollHeight)}); ` +
                  `${r.frames} frames, median ${r.median.toFixed(1)}ms, p95 ${r.p95.toFixed(1)}ms, worst ${r.worst.toFixed(0)}ms, ` +
                  `page long tasks ${r.longN} totalling ${Math.round(r.longMs)}ms; ${refused} step(s) ended more than 8px from where they were written (worst ${Math.round(worstMiss)}px)`,
              )
              if (r.frames >= 8 && r.median > 33.4)
                fail(
                  `scrolling ${mounted} blocks takes ${r.median.toFixed(1)}ms per frame: the rows coming into view cost more than one frame's worth of work`,
                )
              if (r.longN > 2)
                fail(
                  `${r.longN} long task(s) totalling ${Math.round(r.longMs)}ms while scrolling a grown transcript: the page itself stalled the frame clock`,
                )
              if (refused > 3)
                fail(
                  `${refused} of ${steps} scroll steps ended far from where the harness wrote them (worst ${Math.round(worstMiss)}px): something is moving the scroller behind the reader`,
                )
            }
          }
          await invoke('delete_session', { id: sessH }).catch((e) => pageLog(`cleanup fill: ${e}`))
          await useSession(target.id)
        }

        // (a12) the toolbar quotes the number that actually trims history
        //
        // The report read "61 message(s), ~53132 tokens of a 8000 token budget"
        // next to a model picker that said 262144. Two different ceilings were on
        // screen and only one of them was real. The engine now carries the budget
        // and the window side by side; this checks the panel renders both, and
        // that the budget is derived from the window rather than left at a
        // fallback - which is the difference between keeping a conversation and
        // deleting its oldest 57 messages.
        {
          const h = await invoke<HostState>('session_state')
          // Half the stated window, deliberately: the other half is the reply. What this
          // check is for is a budget that was never derived from the window at all — the
          // 32 000 fallback sitting under a 262 144 window, trimming history nobody asked
          // to trim. Assert the derivation, not one core's constant.
          const share = h.model_window ? h.context_budget / h.model_window : 0
          if (h.model_window && (share < 0.25 || share > 1 || h.context_budget === 32_000))
            fail(
              `the engine budgets ${h.context_budget} tokens of a ${h.model_window} token window: ` +
                'that is a fallback, not a derivation, and it trims history nobody asked to trim',
            )
          const btnM = document.querySelector('button[aria-label="Session telemetry"]')
          const row = () => document.querySelector('.mbar-row')
          if (!btnM) fail('the footer has no telemetry button')
          else {
            ;(btnM as HTMLElement).click()
            await new Promise((r) => setTimeout(r, 250))
            const r0 = row()
            const txt = (r0?.textContent ?? '').replace(/\s+/g, ' ').trim()
            const tick = r0?.querySelector('.mbar u') as HTMLElement | null
            const left = tick ? parseFloat(tick.style.left) : NaN
            const want = h.model_window
              ? Math.min(100, (h.context_budget / h.model_window) * 100)
              : NaN
            window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
            if (!r0) fail('the telemetry panel shows no context row')
            // The number that leads is the share of the window the endpoint states; the
            // exact counts live in the tooltip. Both halves are asserted, because a
            // percentage with no counts behind it is a bar chart, and counts with no
            // percentage is what this row used to be.
            else if (!txt.includes('%'))
              fail(
                `the context row leads with something other than the share of the window: "${txt}"`,
              )
            else if (
              !(r0.querySelector('.mval')?.getAttribute('title') ?? '').includes(
                h.context_tokens.toLocaleString(),
              )
            )
              fail(
                `the exact token count (${h.context_tokens}) is not where the row says it lives, in the tooltip: ` +
                  `"${r0.querySelector('.mval')?.getAttribute('title') ?? ''}"`,
              )
            else if (h.context_budget && !txt.includes('trims at'))
              fail(
                `the row shows a window but not the line history is trimmed past (${h.context_budget}): "${txt}"`,
              )
            else if (!Number.isNaN(want) && (Number.isNaN(left) || Math.abs(left - want) > 1.5))
              fail(
                `the trim tick sits at ${left}% of the bar but the budget is ${want.toFixed(1)}% ` +
                  `of the stated window (${h.context_budget} / ${h.model_window})`,
              )
            else
              pageLog(
                `context readout ok (${h.context_tokens} of ${h.model_window ?? 'unreported'}, ` +
                  `trim line at ${Number.isNaN(want) ? 'n/a' : want.toFixed(1)}%, "${txt}")`,
              )
          }
        }

        // (a12b) the context ceiling is a setting, and the setting is what the meter says
        //
        // The ceiling used to be arithmetic nobody could reach: half the window, on whoever
        // computed it. Now it can be pinned, refused, and freed — and the config file, the
        // live meter and (on the pi core) the line the model is told to fold at have to agree.
        // This drives the control the way a person drives it, from the panel: a check that
        // called the verb directly would measure the backend and leave the page's own state
        // behind, which is what the first version of this check did — it pinned the host and
        // then failed because the row still showed the old number. Reading the host's state
        // is still how the numbers are asserted; the click is how they are produced.
        {
          const h0 = await invoke<HostState>('session_state')
          const win = h0.model_window ?? 0
          if (!win) {
            pageLog(
              'ceiling check skipped: this endpoint stated no window, so a derived ceiling is not knowable',
            )
          } else {
            // The ceiling row is the settings control: the metrics popover shows the meter,
            // not the control, and there is exactly one copy of the markup. So the check
            // opens the page it needs. It used to click the telemetry button and then reach
            // for a copy that only exists inside the settings dialog, which meant the case
            // worked only while some earlier case happened to leave that dialog open - a
            // check that inherits another one's state fails for reasons that have nothing to
            // do with it.
            openSettings('models')
            await renderTick()
            const crowEl = () =>
              [...document.querySelectorAll('[data-set-ceiling]')].find((r) =>
                (r.textContent ?? '').includes('ceiling'),
              ) as HTMLElement | undefined
            const crow = crowEl()
            const ctext = () => (crowEl()?.textContent ?? '').replace(/\s+/g, ' ').trim()
            /**
             * Wait for the ceiling row to say something.
             *
             * The click asks the host; the page learns from the same read the panel does on
             * its own, so the row catches up within a poll cycle. A check that reads the row
             * in the same tick as its own backend poll races the page for no reason - and
             * when it loses, the failure reads like the control is broken. If it still never
             * says it, the message names the page's own numbers, because "the row is stale"
             * and "the page was never told" are different bugs.
             */
            const rowSays = async (want: string) => {
              const until = performance.now() + 4000
              for (;;) {
                if (ctext().includes(want)) return true
                if (performance.now() > until) return false
                await new Promise((r) => setTimeout(r, 100))
              }
            }
            const box = crow?.querySelector('[data-ceiling-input]') as HTMLInputElement | undefined
            const btnNamed = (name: string) =>
              [...(crow?.querySelectorAll('button') ?? [])].find((b) =>
                (b.textContent ?? '').includes(name),
              ) as HTMLButtonElement | undefined
            const pinBtn = btnNamed('Pin')
            const deriveBtn = btnNamed('Derive')
            const sayRow = () =>
              [...document.querySelectorAll('.set-note')].map((n) => n.textContent ?? '').join(' | ')

            if (!crow) fail('the telemetry panel shows no ceiling row')
            else if (!box || !pinBtn || !deriveBtn) fail('the ceiling row has no token field and no Pin / Derive buttons')
            else if (!ctext().includes(h0.budget_pin ? 'pinned by you' : 'derived'))
              fail(`the ceiling row does not say how its number was arrived at: "${ctext()}"`)
            else {
              const pinTo = Math.max(4096, Math.floor(win / 3 / 1024) * 1024)
              box.value = String(pinTo)
              box.dispatchEvent(new Event('input', { bubbles: true }))
              pinBtn.click()
              let h1 = await invoke<HostState>('session_state')
              for (let i = 0; i < 40 && h1.context_budget !== pinTo; i++) {
                await new Promise((r) => setTimeout(r, 100))
                h1 = await invoke<HostState>('session_state')
              }
              if (h1.context_budget !== pinTo)
                fail(`pinning the ceiling to ${pinTo} from the panel never moved the meter (${h1.context_budget})`)
              else if (h1.budget_pin !== pinTo)
                fail(`the host does not say the ceiling is pinned (budget_pin=${h1.budget_pin}); the panel cannot tell a pin from a derivation`)
              else if (!(await rowSays('pinned by you')))
                fail(
                  `the row still does not say the ceiling is pinned: "${ctext()}" (the page itself reads budget_pin=${hostState?.budget_pin ?? 'null'}, context_budget=${hostState?.context_budget ?? 'null'})`,
                )
              else {
                // A ceiling that cannot mean anything is refused in words, and the refusal
                // changes nothing: not the file, not the meter, not the row.
                box.value = '900'
                box.dispatchEvent(new Event('input', { bubbles: true }))
                pinBtn.click()
                await new Promise((r) => setTimeout(r, 400))
                const note = sayRow()
                if (!/floor/i.test(note))
                  fail(`a 900-token ceiling was not refused in words on screen: "${note || '(no message shown)'}"`)
                const h2 = await invoke<HostState>('session_state')
                if (h2.context_budget !== pinTo)
                  fail(`the refused ceiling moved the meter anyway: ${h2.context_budget}`)
                else if (!ctext().includes('pinned by you'))
                  fail(`a refused change altered what the row claims: "${ctext()}"`)
                else {
                  // And back to derived, which is not a number: the line comes out of the
                  // file, so the next start does not re-pin itself.
                  if (!deriveBtn || deriveBtn.disabled) fail('the ceiling row has no live Derive control')
                  else {
                    deriveBtn.click()
                    const wantBack = Math.max(4096, Math.floor(win / 2))
                    // Both halves of the claim have to change, and they change together:
                    // the pin going away and the number being re-derived are one decision.
                    // Polling for the pair (bounded) is honest; polling for one field and
                    // asserting the other is how this case once reported a mismatch the
                    // host had never published.
                    let h3 = await invoke<HostState>('session_state')
                    let waitedMs = 0
                    for (let i = 0; i < 40; i++) {
                      if (h3.budget_pin === null || h3.budget_pin === undefined) {
                        if (h3.context_budget === wantBack) break
                      }
                      await new Promise((r) => setTimeout(r, 100))
                      waitedMs += 100
                      h3 = await invoke<HostState>('session_state')
                    }
                    if (h3.budget_pin) fail(`pressing Derive left a pin in place: ${h3.budget_pin}`)
                    else if (h3.context_budget !== wantBack)
                      fail(
                        `after freeing the pin the meter still reads ${h3.context_budget} after ${waitedMs}ms, not half the stated window (${wantBack})`,
                      )
                    else {
                      await new Promise((r) => setTimeout(r, 250))
                      if (!ctext().includes('derived'))
                        fail(`the row still claims a pin after deriving: "${ctext()}"`)
                      pageLog(
                        `ceiling ok (pinned ${pinTo} from the panel and the meter followed, 900 refused as ` +
                          `"${note.slice(0, 44)}…", derive-it back to ${h3.context_budget} = half of ${win})`,
                      )
                    }
                  }
                }
              }
            }
            window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
          }
        }

        // (a13) a turn that wrote a dozen files shows one line, not a dozen chips
        //
        // Implementing a feature in a real repository touches dozens of files, and
        // every one of them became a chip - which made the one thing worth opening
        // (the standalone page the model also wrote) impossible to find. The engine
        // now labels each produced file as a deliverable or a change, and the
        // transcript gives the two shapes: a chip each, or one expandable count.
        {
          const hostWas = hostState?.session_id ?? 0
          const viewWas = viewId ?? 0
          const sessF = await invoke<number>('new_session', { title: 'selftest attachment flood' })
          const bF = await turnCount()
          await invoke('start_turn', {
            text: 'Selftest attachment flood: implement five modules under src/ and a standalone panel page at the workspace root.',
            session: sessF,
          }).catch((e) => pageLog(`selftest note: flood turn failed: ${e}`))
          if (!(await waitTurnDone(bF, 120000))) fail('the attachment flood turn never finished')
          await useSession(sessF)
          await new Promise((r) => setTimeout(r, 400))
          const storedF = await invoke<any>('session_messages', { id: sessF, limit: 40 })
          const produced = (storedF.messages ?? []).flatMap((m: any) => m.files ?? [])
          const kinds = produced.map((f: any) => f.kind ?? 'missing')
          const strip = [...document.querySelectorAll('.filestrip')].pop() as HTMLElement | undefined
          if (produced.length < 6) {
            fail(
              `the flood turn produced ${produced.length} files (expected 6): ${produced
                .map((f: any) => f.path)
                .join(', ') || 'nothing'}` +
                ' - the fixture did not run as written',
            )
          } else if (!kinds.includes('edit') || !kinds.includes('artifact')) {
            fail(`the engine labelled none of the ${produced.length} produced files: ${kinds.join(',')}`)
          } else if (!strip) {
            fail('the flood turn shows no produced-files strip at all')
          } else {
            const topChips = [...strip.querySelectorAll(':scope > .filechip')] as HTMLElement[]
            const roll = strip.querySelector('.fileroll') as HTMLElement | null
            const head = roll?.querySelector('.fileroll-head') as HTMLElement | null
            const list = roll?.querySelector('.fileroll-list') as HTMLElement | null
            const panel = topChips.find((c) => (c.dataset.path ?? '').endsWith('selftest-flood-panel.html'))
            if (!panel) {
              fail(
                `the standalone page is not its own chip (chips: ${
                  topChips.map((c) => c.dataset.path).join(', ') || 'none'
                })`,
              )
            } else if (!roll || !head || !list) {
              fail(
                `the ${kinds.filter((k: string) => k === 'edit').length} changed files are individual chips: a codebase ` +
                  'change buries the deliverable in the same turn',
              )
            } else if (!/5 files written/.test(head.textContent ?? '')) {
              fail(`the roll does not count the changes: "${(head.textContent ?? '').trim()}"`)
            } else {
              const closedAtFirst = list.hidden
              head.click()
              await new Promise((r) => setTimeout(r, 150))
              const rolled = [...list.querySelectorAll('.filechip')] as HTMLElement[]
              const cs = getComputedStyle(head)
              if (!closedAtFirst) fail('the rolled list is expanded before anyone asked for it')
              else if (rolled.length !== 5)
                fail(`expanding the roll reveals ${rolled.length} of the changed files, not 5`)
              else if (!rolled.every((c) => (c.dataset.path ?? '').includes('selftest-flood')))
                fail('a rolled file lost the path that makes it clickable')
              else if (cs.backgroundColor === 'rgba(0, 0, 0, 0)' || cs.backgroundColor === 'transparent')
                fail('the roll has no fill: it reads as loose text rather than a control')
              else
                pageLog(
                  `attachment roll ok (${produced.length} files: 1 deliverable chip + a roll of ` +
                    `${rolled.length} changes, every path reachable on expand)`,
                )
            }
          }
          // Leave the way it was found, in the order that cannot strand the host:
          // a session the engine is standing in cannot be deleted underneath it -
          // that is how a previous version of this check left the host on session 0
          // and broke every check that followed.
          if (hostWas) await invoke('switch_session', { id: hostWas }).catch((e) => pageLog(`switch back failed: ${e}`))
          await invoke('delete_session', { id: sessF }).catch((e) => pageLog(`delete_session failed: ${e}`))
          if (viewWas) await resyncView(viewWas)
        }

        // (a14) an engine note is quiet to read and obvious to find
        //
        // Notes about the app itself ("resumed session 14: 160 message(s)...") used
        // to render as full-size body text, indistinguishable from the answer, which
        // is both intrusive and easy to miss. They now sit in a tinted block at a
        // smaller size. The CSS is part of the contract, so the check reads the
        // computed style rather than the class name, and also holds the engine to its
        // own rule: a note may not claim a cache state it cannot see.
        {
          const hostWas2 = hostState?.session_id ?? 0
          const viewWas2 = viewId ?? 0
          const sessN = await invoke<number>('new_session', { title: 'selftest notes' })
          const bN = await turnCount()
          await invoke('start_turn', { text: 'Selftest notes: what is 6 times 7?', session: sessN }).catch((e) =>
            pageLog(`selftest note: seed turn failed: ${e}`),
          )
          if (!(await waitTurnDone(bN, 90000))) fail('the note seed turn never finished')
          // Leave and come back: that is the switch the engine announces with the
          // resumed-history note, and it is the note this check styles.
          const away = await invoke<number>('new_session', { title: 'selftest notes away' })
          await useSession(away)
          await useSession(sessN)
          const findNoteToast = () => document.querySelector('.toast.note') as HTMLElement | null
          let noteEl = findNoteToast()
          // The note and the repaint race: the switch emits both. It is remembered by the
          // transcript and re-announced after a repaint; give that a moment before deciding
          // it never arrived.
          for (let i = 0; !noteEl && i < 12; i++) {
            await new Promise((r) => setTimeout(r, 150))
            noteEl = findNoteToast()
          }
          const answerBody = [
            ...document.querySelectorAll('.blk-answer .blk-body'),
          ].pop() as HTMLElement | null
          const inStream = [...document.querySelectorAll('.blk-note')].some((n) =>
            (n.textContent ?? '').includes('resumed session'),
          )
          if (!noteEl) {
            fail('the resumed-history note never reached the screen (buffered, or dropped by the repaint)')
          } else if (inStream) {
            fail('an app-level note is painted inside the transcript: it belongs in the chrome now')
          } else if (!answerBody) {
            fail('no answer block on screen to compare a note against')
          } else {
            const nCs = getComputedStyle(noteEl)
            const aCs = getComputedStyle(answerBody)
            const nSize = parseFloat(nCs.fontSize)
            const aSize = parseFloat(aCs.fontSize)
            const text = (noteEl.textContent ?? '').replace(/\s+/g, ' ').trim()
            // Both cores announce what reopening this conversation cost, and they describe different
            // facts. The rust core resumes a stored session; the pi core often cannot, because the
            // conversation has no pi thread of its own and pi must start one from the stored
            // transcript. Either sentence is the truth for that core - the check is that a note
            // arrived in the chrome and is styled as chrome, not which words the core chose.
            if (!/resumed session|no pi thread of its own yet|starts this one from scratch/i.test(text))
              fail(`the note on screen is not the resumed-history note: "${text}"`)
            else if (!(nSize <= aSize))
              fail(`a note is set at ${nSize}px against an answer at ${aSize}px: it shouts over the answer`)
            else if (nCs.backgroundColor === 'rgba(0, 0, 0, 0)' || nCs.backgroundColor === 'transparent')
              fail('the note notice has no tinted plate: it is a bare line of text floating in the chrome')
            else if (parseFloat(nCs.borderLeftWidth) < 2)
              fail(`the note notice's rule is ${nCs.borderLeftWidth} wide: no edge to find it by`)
            else if (/\bcold\b|\bwarm\b|re-?prefill/i.test(text))
              fail(`the note asserts a cache state the engine cannot see: "${text}"`)
            else
              pageLog(
                `note notice ok (${nSize}px chrome text against ${aSize}px answers, ${nCs.backgroundColor} plate, ` +
                  `${nCs.borderLeftWidth} rule, outside the transcript: "${text.slice(0, 60)}...")`,
              )
          }
          // switch the host out before deleting: see the note in the check above
          if (hostWas2) await invoke('switch_session', { id: hostWas2 }).catch((e) => pageLog(`switch back failed: ${e}`))
          for (const id of [sessN, away])
            await invoke('delete_session', { id }).catch((e) => pageLog(`delete_session failed: ${e}`))
          if (viewWas2) await resyncView(viewWas2)
        }

        // (a15) an image copied by another program reaches the composer
        //
        // The webview cannot see it: a screenshot sits in the Wayland/X11 selection,
        // and WebKitGTK's paste event carries no file and no image item for it. The
        // shell reads the selection instead. The fixture is placed on the clipboard
        // by scripts/selftest.sh (which puts whatever was there back afterwards).
        {
          const noClip = import.meta.env.VITE_SELFTEST_NOCLIP === '1'
          // There is no clipboard button. A screenshot reaches the composer the way it does in every
          // other program - paste it - and the handler that asks the shell what the selection holds
          // is the same one either way. So the harness performs the gesture a reader performs and the
          // shipped handler does the rest.
          const cbox = document.querySelector('.cbox') as HTMLElement | null
          const why = (document.querySelector('.att-err')?.textContent ?? '').trim()
          if (!cbox) {
            fail('there is no composer on the page to paste into')
          } else if (!isTauri()) {
            fail('the clipboard check ran somewhere other than the desktop shell')
          } else if (noClip) {
            pageLog('clipboard paste SKIPPED: this machine has no wl-clipboard, so nothing was placed on the selection')
          } else {
            // The selection belongs to the desktop, not to this run. A person copying
            // something mid-run, or an earlier check that copies an answer, replaces the
            // fixture image with text, and then this check reports "nothing sendable" about a
            // fixture that stopped existing. Ask the app to put the board's own file back on
            // the selection at the moment it is needed; the verb refuses unless the board
            // named that file in CWOFFEE_SELFTEST_CLIP_IMG, so a product build cannot be
            // talked into overwriting what the user copied.
            // Bounded on purpose. This reaches outside the page into the desktop, and an
            // external tool that never answers must cost one honest skip, not the whole board:
            // a `wl-copy` that forks a resident clipboard owner once held this request open
            // for the remaining 15 minutes of a run and the harness printed no verdict.
            const staged = await Promise.race([
              invoke<{ mime: string; bytes: number }>('selftest_stage_clipboard')
                .then((v) => ({ v, why: '' }))
                .catch((e) => ({ v: null as null | { mime: string; bytes: number }, why: String(e) })),
              new Promise<{ v: null; why: string }>((res) =>
                setTimeout(() => res({ v: null, why: 'the app never answered the staging request (4s) — the helper it calls did not return' }), 4000),
              ),
            ])
            if (!staged.v) {
              // A skip with a reason, and no substitute: firing a synthetic paste event with a
              // made-up file would exercise our handler and not the desktop path, which is the
              // whole thing this check exists for.
              pageLog(`clipboard paste SKIPPED: ${staged.why || 'the app refused to stage a fixture'}`)
            } else {
              const stagedInfo = staged.v
              pageLog(`clipboard fixture re-staged for this check: ${stagedInfo.mime}, ${stagedInfo.bytes} bytes`)
            const had = pending.length
            try {
              cbox!.dispatchEvent(new ClipboardEvent('paste', { bubbles: true, cancelable: true }))
            } catch (e) {
              pageLog(`the paste gesture could not be dispatched: ${e}`)
            }
            const grew = await waitFor('the clipboard image to attach', () => pending.length > had, 9000)
            const pend = pending[pending.length - 1]
            if (!grew || !pend) {
              fail(`pasting the clipboard attached nothing${why ? `: ${why}` : ''}`)
            } else {
              // The bytes are the proof: a PNG magic number and the fixture's
              // dimensions, not just "something appeared in the composer".
              const raw = atob(pend.b64)
              // 0x89 P N G \r \n 0x1a \n. atob decodes to a Latin-1 string, so the
              // magic number has to be read by code point, not by startsWith: the
              // first byte is not printable, and the earlier version of this check
              // failed on a payload that had arrived perfectly.
              const isPng =
                raw.charCodeAt(0) === 0x89 &&
                raw.charCodeAt(1) === 0x50 &&
                raw.charCodeAt(2) === 0x4e &&
                raw.charCodeAt(3) === 0x47 &&
                raw.length > 40
              if (pend.mime !== 'image/png') fail(`the clipboard image came in as ${pend.mime}, not image/png`)
              else if (!isPng) fail(`the pasted payload is not a PNG (${raw.length} bytes, first bytes ${raw.charCodeAt(0)},${raw.charCodeAt(1)},${raw.charCodeAt(2)},${raw.charCodeAt(3)})`)
              else if (raw.length > 2 * 1024 * 1024)
                fail(`the pasted payload is ${raw.length} bytes, past the engine's attachment ceiling`)
              else if (!/^clipboard-\d{6}\.png$/.test(pend.name))
                fail(`the pasted image is named "${pend.name}": a pasted screenshot should say where it came from`)
              else if (!document.querySelector('.attrow .attchip img'))
                fail('the pasted image has no preview in the composer')
              else
                pageLog(
                  `clipboard paste ok (${raw.length} bytes of image/png read off the desktop selection, ` +
                    `named ${pend.name}, previewed in the composer)`,
                )
              dropAttachment(pending.length - 1)
            }
            }
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
            else {
              // The card closing is half the feature. The other half is the picks arriving
              // where the model reads them. There is deliberately no tool card for `ask_user`:
              // the renderer turns one question into one card, and a second card next to it
              // saying "ask_user running" reads as a bug. So the proof is read from the store,
              // where the tool row carries the engine's own restatement — "User chose: 1:
              // Engineers" — and that is the text the endpoint has to see in the prompt before
              // it will serve the scripted step.
              // `session_messages` answers with a page, not a list — `{ messages, has_older }`
              // — and paging is capped, so ask for a page wide enough to hold the run.
              const page = await invoke<{ messages?: { content?: unknown }[] }>('session_messages', {
                id: viewId,
                limit: 200,
              }).catch(() => ({ messages: [] }))
              const rows = page.messages ?? []
              const all = rows.map((r) => (typeof r.content === 'string' ? r.content : JSON.stringify(r.content ?? ''))).join('\n')
              const picks = ['User chose: 1: Engineers', 'User chose: 2: Two pages']
              const missing = picks.filter((t) => !all.includes(t))
              const shown = pairs.map((_, i) => `${questions[i]} -> ${answers[i]}`)
              if (missing.length)
                fail(`the picks never reached the stored transcript; missing ${JSON.stringify(missing)} from ${rows.length} row(s), ${all.length} chars`)
              else if (!answers.some((a) => a.includes('Engineers')) || !answers.some((a) => a.includes('Two pages')))
                fail(`the settled card does not show the picks the user made: ${JSON.stringify(shown)}`)
              else
                pageLog(
                  `scripted ask set ok (one tool call, ${questions.length} questions, one card; settled "${receipt}"; `
                  + `the stored transcript carries both restated picks: ${JSON.stringify(picks)})`,
                )
            }
          }
        }

        // (c2) the working surface: one accordion per turn, the saying below it
        //
        // Driven through transcript.apply(), which is the same entry point the live
        // event stream uses. The claim under test is the renderer's grouping rule, so
        // the events are the app's own shapes rather than a re-creation of them.
        //
        // The rule used to be "a paragraph of answer splits the working into two
        // accordions". It is now "one accordion per turn, everything the model said underneath
        // it", because two accordions above one answer is what made a turn look cluttered —
        // and because a turn reads as one block of working followed by what came of it. The
        // events themselves are untouched: this is display only, and stored history stays in
        // the order it arrived, which is what keeps a prompt cache matching turn after turn.
        {
          transcript?.clear()
          transcript?.apply({ kind: 'turnStart', d: { turn_id: 31, model: 'selftest', session_id: 0 } })
          transcript?.addUser('Find the number, then tell me.')
          transcript?.apply({ kind: 'reasoning', d: 'the file should hold it' })
          transcript?.apply({ kind: 'toolCallBegin', d: { id: 'a1', name: 'read_file' } })
          transcript?.apply({ kind: 'toolCallReady', d: { id: 'a1', args: { path: 'note.md' } } })
          transcript?.apply({ kind: 'toolExecOutput', d: { id: 'a1', stream: 'stdout', chunk: 'the number is 7' } })
          transcript?.apply({ kind: 'toolExecEnd', d: { id: 'a1', ok: true, exit_code: 0, changes: [] } })
          transcript?.apply({ kind: 'text', d: 'It is in note.md.' })
          transcript?.apply({ kind: 'reasoning', d: 'now check the sheet' })
          transcript?.apply({ kind: 'toolCallBegin', d: { id: 'a2', name: 'sheet_read' } })
          transcript?.apply({ kind: 'toolExecEnd', d: { id: 'a2', ok: true, exit_code: 0, changes: [] } })
          transcript?.apply({ kind: 'text', d: 'Seven, in both.' })
          transcript?.apply({ kind: 'turnEnd', d: { turn_id: 31, finish: 'stop' } })
          await new Promise((r) => setTimeout(r, 220))
          const turnEl = document.querySelector('.turn') as HTMLElement | null
          const runs = [...document.querySelectorAll('.turn > .blk-run')] as HTMLElement[]
          const texts = [...document.querySelectorAll('.turn > .blk-answer')] as HTMLElement[]
          const said = texts.map((t) => t.textContent ?? '')
          const tools = runs[0]?.querySelectorAll('.blk-tool').length ?? 0
          const thinks = runs[0]?.querySelectorAll('.blk-reasoning').length ?? 0
          const kids = turnEl ? [...turnEl.children] : []
          const order = runs.length === 1 && texts.every((t) => kids.indexOf(t) > kids.indexOf(runs[0]))
          if (!turnEl) fail('no turn element to group the working inside')
          else if (runs.length !== 1)
            fail(`one turn, one run accordion: saw ${runs.length} (the model's text split it again)`)
          else if (!order)
            fail('something the model said sits above the working instead of below it')
          else if (said.length !== 2 || !said[0].includes('note.md') || !said[1].includes('Seven'))
            fail(`the message text did not accumulate below the run: ${JSON.stringify(said).slice(0, 120)}`)
          else if (tools !== 2 || thinks !== 2)
            fail(`the run holds ${tools} tool entries and ${thinks} thinking entries, expected 2 and 2`)
          else {
            const l1 = runs[0].querySelector('.run-label')?.textContent ?? ''
            const cnt = runs[0].querySelector('.run-count')?.textContent ?? ''
            if (!/^Thought, then worked/.test(l1))
              fail(`a run that thought and worked is labelled ${JSON.stringify(l1)}`)
            else if (!/4 steps/.test(cnt))
              fail(`the accordion counts ${JSON.stringify(cnt)}, expected "4 steps"`)
            else if (runs[0].classList.contains('open'))
              fail('a finished run is still open once the answer landed')
            else if (runs[0].classList.contains('running'))
              fail('the run reopened for the second batch of work and never settled again')
            const t1 = runs[0].querySelector('.blk-tool') as HTMLElement
            if (getComputedStyle(t1.querySelector('.tool-detail') as Element).display !== 'none')
              fail('an entry inside a closed run is still spilling its body')
            // Nothing was lost by folding: the words are still there to find.
            if (!(t1.querySelector('.tool-out')?.textContent ?? '').includes('the number is 7'))
              fail('folding the run threw the tool output away instead of hiding it')
            // ...and one thing that is *not* in the page by design. The thinking preview was
            // removed, not folded, so these words live in the store and nowhere on screen: a
            // setting, a click or a reload must never bring them back.
            if (document.body.innerText.includes('the file should hold it'))
              fail('a thinking block put its words on screen; the preview was supposed to be gone for good')
            pageLog(
              `run accordion ok (one accordion per turn holding ${tools} calls and ${thinks} thoughts, `
                + `the two paragraphs of answer below it, "${l1}" / ${cnt.trim()}, entries fold to lines, `
                + 'output still in the page, thought words never on it)',
            )

          }
          transcript?.clear()
        }

        // A settled turn keeps the grouping the live turn had. The live shape was right and
        // the reopened one was not: every stored assistant row was painted as an answer,
        // including the rows a tool call leaves with no text at all, each with its own
        // `Copy | Share as markdown | Branch` line under nothing. Those bare strips between
        // the run accordions read as the runs separating again the moment a turn ended - and
        // the reopened transcript is the look people compare their memory against.
        {
          pageLog('case: a settled transcript keeps the run grouping')
          const base = {
            session_id: 1,
            reasoning: '',
            reasoning_signature: '',
            reasoning_ms: 0,
            files: [],
            attachments: [],
            tool_call_id: null,
            tool_calls_json: '',
          }
          // Two turns, each: a silent assistant row carrying a tool call, the tool's answer,
          // then the sentence the model said. `content: ''` is what the engine stores for a
          // message that spent itself entirely on a call.
          const rows: StoredMessage[] = [
            { ...base, id: 1, role: 'user', content: 'first ask', created_at: 1 },
            {
              ...base,
              id: 2,
              role: 'assistant',
              content: '',
              tool_calls_json: '[{"id":"c1","name":"read_file","arguments":"{}"}]',
              created_at: 2,
            },
            { ...base, id: 3, role: 'tool', content: 'the file said seven', tool_call_id: 'c1', created_at: 3 },
            { ...base, id: 4, role: 'assistant', content: 'Let me keep going.', created_at: 4 },
            { ...base, id: 5, role: 'user', content: 'second ask', created_at: 5 },
            {
              ...base,
              id: 6,
              role: 'assistant',
              content: '',
              tool_calls_json: '[{"id":"c2","name":"web_search","arguments":"{}"}]',
              created_at: 6,
            },
            { ...base, id: 7, role: 'tool', content: 'three results', tool_call_id: 'c2', created_at: 7 },
            { ...base, id: 8, role: 'assistant', content: 'Done.', created_at: 8 },
          ]
          transcript?.loadHistory(rows)
          await new Promise((r) => setTimeout(r, 300))
          const answers = [...document.querySelectorAll('.blk-answer')] as HTMLElement[]
          const quiet = answers.filter(
            (a) => (a.querySelector('.blk-body')?.textContent ?? '').trim() === '',
          )
          const runs = [...document.querySelectorAll('.blk-run')] as HTMLElement[]
          const acts = document.querySelectorAll('.turn-acts').length
          if (quiet.length)
            fail(
              `${quiet.length} answer block(s) on screen say nothing at all: an assistant row with no text still paints a ` +
                `label and a `+
                `footer, and that strip between two run accordions is exactly what reads as the run splitting open again`,
            )
          else if (answers.length !== 2)
            fail(`${answers.length} answer block(s) for 2 spoken message(s): the transcript invented or dropped an answer`)
          else if (answers.some((a) => a.closest('.blk-run')))
            fail('an answer block sits inside a run accordion, so it divides nothing')
          else if (runs.length !== 2)
            fail(`${runs.length} run accordion(s) for 2 tool groups: the grouping did not survive the reload`)
          else if (runs.some((r) => !r.querySelector('.run-body > *'))) fail('a stored run accordion holds nothing')
          else if (runs.some((r) => (r.querySelector('.run-label')?.textContent ?? '') === ''))
            fail('a stored run header is blank, which reads as an empty accordion')
          else if (acts !== 2)
            fail(
              `${acts} copy/share line(s) for 2 turn(s): one under every message is the wall this replaced, and none is a ` +
                `lost affordance - the live turn offers them`,
            )
          else
            pageLog(
              `settled grouping ok (2 spoken rows → 2 blocks, 2 silent rows painted nothing, 2 runs still holding their ` +
                `steps, 2 footer lines placed under their turns)`,
            )
        }

        // The display setting is driven through the control and checked against the store,
        // because it is the app's setting rather than this tab's: a check that called the
        // verb behind the switch would measure the backend and miss the screen.
        {
          pageLog('case: the Transcript section of Settings drives the runs')
          openSettings('display')
          await new Promise((r) => setTimeout(r, 300))
          const row = document.querySelector('[data-pref="verbose_blocks"]') as HTMLElement | null
          if (!row) fail('Settings > Transcript has no verbose_blocks row: the screen is not rendering the host\'s list')
          else {
            // The words on the control come from Rust, so the client cannot advertise a
            // setting the host does not implement.
            const lbl = row.querySelector('[data-pref-label]')?.textContent ?? ''
            if (!/step/i.test(lbl)) fail(`the pref row's label did not come from the host: "${lbl}"`)
            const segOn = row.querySelector('[data-seg-value="on"]') as HTMLButtonElement | null
            const segOff = row.querySelector('[data-seg-value="off"]') as HTMLButtonElement | null
            if (!segOn || !segOff || segOn.disabled)
              fail('the pref row has no live on/off segments')
            else {
              segOn.click()
              await new Promise((r) => setTimeout(r, 500))
              toast = null
              await frame(40)
              const host = await invoke<any[]>('ui_prefs')
              const vb = (host ?? []).find((v) => v.key === 'verbose_blocks')
              if (!vb || vb.value !== 'on')
                fail(`the control said on, the store says ${JSON.stringify(vb?.value)}: the switch and the app disagree`)
              else {
                const runs = [...document.querySelectorAll('.blk-run')] as HTMLElement[]
                const folded = runs.filter((r) => !r.classList.contains('show-out'))
                const detailVisible = [...document.querySelectorAll('.tool-detail')].some(
                  (d) => getComputedStyle(d).display !== 'none',
                )
                if (runs.length && folded.length)
                  fail(`${folded.length} of ${runs.length} run(s) ignored the setting and stayed folded`)
                else if (!detailVisible)
                  fail('the setting is on and every run claims to show its steps, but no tool output is actually visible')
                else {
                  segOff.click()
                  await new Promise((r) => setTimeout(r, 500))
                  toast = null
                  await frame(40)
                  const back = (await invoke<any[]>('ui_prefs'))?.find((v) => v.key === 'verbose_blocks')
                  const still = [...document.querySelectorAll('.blk-run')].filter((r) =>
                    r.classList.contains('show-out'),
                  )
                  if (back?.value !== 'off') fail(`turning it back off did not reach the store: ${JSON.stringify(back?.value)}`)
                  else if (still.length) fail(`${still.length} run(s) stayed open after the setting went back to off`)
                  else pageLog('display setting ok (the control writes the store, the runs follow both ways, the wording is the host\'s)')
                }
              }
            }
            closeSettings()
            await new Promise((r) => setTimeout(r, 200))
          }
        }

        // The device-facing setting is a promise about a process that does not exist yet, so two
        // things have to be true on screen: the row says *which* process would be started, and the
        // toggle reaches the store the app reads at close. The page it renders on is asserted too -
        // a pref filed under the wrong group is a control that appears nowhere, and a pref rendered
        // on two pages is two controls for one setting.
        {
          pageLog('case: Settings > Devices can keep the gateway alive')
          const plan = await invoke<GatewayPlan>('gateway_handoff_plan')
          if (!plan || typeof plan.port !== 'number') fail(`gateway_handoff_plan answered ${JSON.stringify(plan)}`)
          const prefs0 = await invoke<any[]>('ui_prefs')
          const keep0 = (prefs0 ?? []).find((p) => p.key === 'gateway_on_exit')
          if (!keep0) fail('the host has no gateway_on_exit pref: the toggle would be wired to nothing')
          else if (keep0.group !== 'devices')
            fail(`gateway_on_exit sits in group ${JSON.stringify(keep0.group)}: the Devices page would not render it`)
          else if (keep0.value !== 'off')
            fail(`gateway_on_exit starts as ${JSON.stringify(keep0.value)}: a gateway that outlives the window has to be chosen, not inherited`)
          else {
            openSettings('devices')
            await new Promise((r) => setTimeout(r, 300))
            const row = document.querySelector('[data-gw-keep]') as HTMLElement | null
            if (!row) fail('Settings > Devices has no keep-gateway row: the pref is not rendered where the setting lives')
            else {
              const lbl = row.querySelector('[data-gw-keep-label]')?.textContent ?? ''
              if (!/window/i.test(lbl)) fail(`the row's label did not come from the host: "${lbl}"`)
              const planLine = row.querySelector('[data-gw-keep-plan]')?.textContent ?? ''
              if (!plan.binary)
                fail(`no cwoffee-gateway next to this app, so the setting cannot be honoured: "${planLine}"`)
              else if (!planLine.includes(plan.binary))
                fail(`the row does not name the process it would start (${plan.binary}): "${planLine}"`)
              else if (plan.log && !planLine.includes(plan.log))
                fail(`the row does not say where the child's own lines go (${plan.log}): "${planLine}"`)
              const sw = row.querySelector('[role="switch"]') as HTMLButtonElement | null
              if (!sw || sw.getAttribute('aria-checked') !== 'false')
                fail('the keep-gateway switch is missing, or it is not off before the check touches it')
              else {
                sw.click()
                await new Promise((r) => setTimeout(r, 600))
                toast = null
                const on1 = (await invoke<any[]>('ui_prefs'))?.find((p) => p.key === 'gateway_on_exit')
                if (on1?.value !== 'on') fail(`the switch said on, the store says ${JSON.stringify(on1?.value)}`)
                else if (!String(planLine).includes(`:${plan.port}`))
                  fail(`the row names a port other than this gateway's (${plan.port}): "${planLine}"`)
                else {
                  openSettings('display')
                  await new Promise((r) => setTimeout(r, 250))
                  if (document.querySelector('[data-gw-keep]'))
                    fail('gateway_on_exit renders under Appearance as well: one setting, two controls')
                  openSettings('devices')
                  await new Promise((r) => setTimeout(r, 250))
                  const sw2 = document.querySelector('[data-gw-keep] [role="switch"]') as HTMLButtonElement | null
                  if (!sw2 || sw2.getAttribute('aria-checked') !== 'true')
                    fail('the switch did not read back as on after the store said on')
                  else {
                    sw2.click()
                    await new Promise((r) => setTimeout(r, 600))
                    toast = null
                    const back = (await invoke<any[]>('ui_prefs'))?.find((p) => p.key === 'gateway_on_exit')
                    if (back?.value !== 'off')
                      fail(`turning it back off did not reach the store: ${JSON.stringify(back?.value)}`)
                    else
                      pageLog(
                        `keep-gateway ok (plan names ${plan.binary} on :${plan.port} via ${plan.via}; the control writes the store both ways; one control, one page)`,
                      )
                  }
                }
                closeSettings()
                await new Promise((r) => setTimeout(r, 200))
              }
            }
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
      if (want('tool-card', 'a finished card is one line of English, and nothing else')) {
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
        // ...and the run around them is the thing you open. A stored card is an
        // entry, not a control: nine triangles is what the accordion replaced.
        const histRun = cards[0].closest('.blk-run') as HTMLElement | null
        if (!histRun) fail('a stored tool card sits outside a run accordion')
        else {
          if (histRun.classList.contains('running'))
            fail('a stored run is still marked as running')
          const rlabel = histRun.querySelector('.run-label')?.textContent ?? ''
          if (!/^(Worked|Thought|Thought, then worked)/.test(rlabel))
            fail(`a stored run's header does not read as a finished run: ${JSON.stringify(rlabel)}`)
          const rcount = histRun.querySelector('.run-count')?.textContent ?? ''
          if (!/^\d+ steps$/.test(rcount))
            fail(`a stored run does not count its steps: ${JSON.stringify(rcount)}`)
          if (getComputedStyle(histRun.querySelector('.run-body') as Element).display !== 'none')
            fail('a settled run is left open, which is the wall of cards this replaced')
          // Opening the run shows the entries as lines; it does not show their bodies.
          ;(histRun.querySelector('.blk-head') as HTMLElement).click()
          await new Promise((r) => setTimeout(r, 200))
          if (getComputedStyle(histRun.querySelector('.run-body') as Element).display === 'none')
            fail('clicking the run header did not open the accordion')
          if (getComputedStyle(cards[0].querySelector('.tool-detail') as Element).display !== 'none')
            fail('opening a run also spilled its entries\' contents')
          // The one control that does reveal them.
          transcript?.setVerbose(true)
          await new Promise((r) => setTimeout(r, 200))
          if (getComputedStyle(cards[0].querySelector('.tool-detail') as Element).display === 'none')
            fail('verbose blocks did not reveal what the run produced')
          if (getComputedStyle(cards[1].querySelector('.tool-detail') as Element).display === 'none')
            fail('verbose blocks revealed only one entry: the setting is about the whole run')
          transcript?.setVerbose(false)
          await new Promise((r) => setTimeout(r, 200))
          if (getComputedStyle(cards[0].querySelector('.tool-detail') as Element).display !== 'none')
            fail('verbose blocks off did not fold the entries back to lines')
        }

        // A stored thought is a duration line too: the reasoning text is in the row and never in
        // the page. Counting it is what lets a session reopened hours later say the same thing
        // about the thought that was said while it was live.
        const rline = document.querySelector('.blk-reasoning') as HTMLElement | null
        if (!rline) fail('no thinking line in the stored transcript')
        else {
          if (rline.querySelectorAll('.blk-body').length)
            fail('a stored thought carries a body element: the reasoning text reached the page')
          const rlabel = rline.querySelector('.blk-head')?.textContent ?? ''
          if (!/^Thought for /i.test(rlabel)) fail(`thinking header is not a duration: ${JSON.stringify(rlabel)}`)
          if (!/\b\d+ words\b/.test(rlabel) && !/6 words/.test(rlabel))
            fail(`a stored thought of six words does not say so: ${JSON.stringify(rlabel.trim())}`)
          if (document.body.innerText.includes('First the file, then the line inside it.'))
            fail('the stored reasoning text is on screen: the preview is supposed to be gone for good')
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
          // One name per turn, pinned in the gutter: the element that carries it is the turn's
          // marker, not the answer block's own label.
          const lab = document.querySelector('.turn-who-cwoffee .who-name') as HTMLElement
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
      //
      // This check used to stream its words as reasoning, because that was the text that
      // arrived fastest and longest. It now streams them as the answer: a thinking block has
      // no text nodes at all any more (see `thoughtLine` in transcript.ts), so the reveal
      // lives in one place, and a check that watched the wrong place would quietly prove
      // nothing while the feature it guards stayed broken.
      if (want('reveal', 'the streaming reveal: append-only, word by word, never rewritten')) {
        transcript?.clear()
        transcript?.apply({ kind: 'turnStart', d: { turn_id: 1, model: 'selftest', session_id: 0 } })
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
        // How many of this check's samples landed on a real paint. Opacity and
        // filter are only evidence while the compositor is advancing animations.
        let animPaints = 0
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
          const h = document.querySelector('.blk-answer .blk-body .live-para') ?? document.querySelector('.blk-answer .blk-body')
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
          transcript?.apply({ kind: 'text', d })
          // One paint per delta: the reveal unit is whatever landed since the last
          // paint, and a check that applies them in one microtask tests one chunk.
          // Spaced out so a later paint has words from an earlier one that have
          // finished: that is exactly the population the at-rest checks need.
          const painted = await frame(150)
          if (painted) animPaints += 1
          transcript?.flushNow()
          const body = document.querySelector('.blk-answer .blk-body') as HTMLElement
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
          if (painted) judged += atRest.length
          for (const t of painted ? atRest.slice(-8) : []) {
            const cs = getComputedStyle(t)
            if (cs.filter !== 'none') fail(`a settled word is still under a filter: ${cs.filter}`)
            if (Number(cs.opacity) < 0.98)
              fail(`a word from an earlier arrival sits at opacity ${cs.opacity} while new words land`)
          }
          // The other direction: a reveal nobody can see is not a feature. Counting
          // words mid-fade right after a paint is how "subtle but noticeable" gets
          // settled with a number instead of an opinion.
          const inFlight = painted
            ? tins.filter((t) => Number(getComputedStyle(t).opacity) < 0.9).length
            : 0
          if (inFlight > maxInFlight) maxInFlight = inFlight
          if (inFlight > 24)
            fail(`${inFlight} words fading at once: the newest line is grey for lines of reading`)
        }
        const body = document.querySelector('.blk-answer .blk-body') as HTMLElement
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
        // Both claims are about animation, so both need animation to be observable.
        if (animPaints < 2) {
          blindSkips++
          pageLog(
            `selftest note: skipped the reveal-visibility and settled-word claims; this check saw ` +
              `${animPaints} real paint(s), and an un-composited window freezes every reveal at its start`,
          )
        } else if (maxInFlight < 4)
          fail(`only ${maxInFlight} words were ever mid-fade at once: the reveal is invisible`)
        if (animPaints >= 2 && judged < 4)
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
        await frame(40)
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
        transcript?.apply({ kind: 'turnStart', d: { turn_id: 2, model: 'selftest', session_id: 0 } })
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
          await frame(40)
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
          await frame(40)
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
          await frame(40)
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
          await frame(40)
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
            await frame(40)
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
        await frame(40)
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
        // Wait on the frame clock so the window below is measurable: if the page
        // painted nothing in that window, "still half-drawn" says nothing about the
        // reveal, so the claim is skipped by name rather than failed.
        const tailPainted = await frame(400)
        const faint = [...document.querySelectorAll('.blk-answer .tin')].filter(
          (t) => Number(getComputedStyle(t).opacity) < 0.9,
        ).length
        if (faint && tailPainted)
          fail(`${faint} word spans are still half-drawn after 400ms: the ghost tail is back`)
        else if (faint) {
          blindSkips++
          pageLog(
            `selftest note: skipped the ghost-tail claim; ${faint} span(s) sat at their start values ` +
              'and the page delivered no paint in 400ms, so animation progress was not observable',
          )
        }
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
        transcript?.apply({ kind: 'turnStart', d: { turn_id: 3, model: 'selftest', session_id: 0 } })
        transcript?.apply({ kind: 'text', d: 'The first words of the answer are landing ' })
        await frame(40)
        transcript?.flushNow()
        const alignPx = (where: string) => {
          const b = document.querySelector('.blk-answer .blk-body') as HTMLElement
          const who = document.querySelector('.turn-who-cwoffee .who-name') as HTMLElement
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
        transcript?.apply({ kind: 'turnStart', d: { turn_id: 4, model: 'selftest', session_id: 0 } })
        transcript?.apply({ kind: 'text', d: '## The Copy\n\nThe photocopier in the basement started printing again ' })
        await frame(40)
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
        transcript?.apply({ kind: 'turnStart', d: { turn_id: 7, model: 'selftest', session_id: 0 } })
        transcript?.apply({ kind: 'text', d: 'The photocopier printed ' })
        await frame(40)
        transcript?.flushNow()
        const body = document.querySelector('.blk-answer .blk-body') as HTMLElement
        const para = body?.querySelector('.live-para') as HTMLElement | null
        const before = para?.lastElementChild as HTMLElement | null
        const bx = before?.getBoundingClientRect().left ?? 0
        if (!para || !before) fail('the streaming paragraph did not open')
        else {
          transcript?.apply({ kind: 'text', d: '**the copy** and `git status` quietly ' })
          await frame(40)
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
        transcript?.apply({ kind: 'turnStart', d: { turn_id: 11, model: 'selftest', session_id: 0 } })
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
        const line = document.querySelector('.blk-reasoning .blk-head')
        const ansBody = document.querySelector('.blk-answer .blk-body')
        // The bug this guarded was a drain problem: thinking words wandering into the answer when
        // the answer began in the same frame. The claim still holds, and is now stronger: the
        // thinking words are not in the answer, not in the thinking line, and not on the page at
        // all - which is the contract of a thought with no preview.
        if (!line) fail('a turn that thought left no thinking line')
        else if ((line.textContent ?? '').includes(prose))
          fail('the thinking prose was painted into the thinking line')
        else if (!ansBody || !ansBody.querySelector('table'))
          fail('a table that started in the same frame as the thinking did not become a table')
        else if ((ansBody.textContent ?? '').includes(prose))
          fail('the thinking prose was painted into the answer block')
        else if (document.body.innerText.includes(prose))
          fail('the thinking prose reached the page')
        else
          pageLog('switch drain ok (the answer kept its table; the thinking words stayed off the page)')
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
        transcript?.apply({ kind: 'turnStart', d: { turn_id: 3, model: 'selftest', session_id: 0 } })
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
        const pumpTimeouts = { n: 0 }
        const pump = () =>
          new Promise<void>((res) => {
            // rAF is the only clock this check may sample - but rAF cannot report its own absence.
            // On a window the compositor is not painting, a bare `requestAnimationFrame` never fires
            // and the chain parks: three runs ended with the page silent, the web process idle, and no
            // verdict, because the follow check's first pump never came back. So the pump has a floor,
            // and the loops below stop early once the floor is what is answering.
            let done = false
            const finish = (painted: boolean) => {
              if (done) return
              done = true
              clearTimeout(timer)
              if (!painted) pumpTimeouts.n += 1
              res()
            }
            const timer = setTimeout(() => finish(false), 420)
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
              finish(true)
            })
          })
        for (let i = 0; i < filler.length && pumpTimeouts.n < 12; i += 96) {
          transcript?.apply({ kind: 'text', d: filler.slice(i, i + 96) })
          // ~250ms between arrivals: 96 characters is roughly what this endpoint
          // produces in that time (30-45 words/s). Feeding at eight times real
          // speed measures the harness's own queue, not the reader's experience.
          for (let f = 0; f < 15 && pumpTimeouts.n < 12; f++) await pump()
          transcript?.flushNow()
          for (let f = 0; f < 3 && pumpTimeouts.n < 12; f++) await pump()
        }
        // Let the glide finish, then judge where it ended up.
        for (let t = 0; t < 60 && pumpTimeouts.n < 24; t++) {
          await pump()
          if (sc.scrollHeight - sc.clientHeight - sc.scrollTop <= 1) break
        }
        const away = sc.scrollHeight - sc.scrollTop - sc.clientHeight
        const blindFrames = pumpTimeouts.n >= 12 || frames < 40
        if (blindFrames) {
          blindSkips++
          pageLog(
            `selftest note: skipped the follow-cadence verdict; ${pumpTimeouts.n} pump(s) fell to the 420ms floor and only ${frames} real frame(s) were sampled ` +
              `(${offScreen() ?? 'the page looked laid out, so the window was most likely not being composited'})`,
          )
        } else if (frames < 40) fail(`the follow check sampled ${frames} frames: it measured nothing`)
        // Half a line. A step is at most max(8px, 18% of the gap) and the gap is
        // bounded below, so anything past 12px means more than one step landed in
        // one frame - and every settled word on screen changed place at once, which
        // is what the user calls a blink.
        // Half a line steady, more only when the view has fallen behind a block
        // that landed all at once, and even then gathering speed frame by frame
        // (FOLLOW_ACCEL). A large step reached by a ramp is motion; a large step
        // that appears out of nowhere is the blink.
        if (!blindFrames && worstStep > 24)
          fail(
            `the transcript moved ${Math.round(worstStep)}px in one frame: the follow lurches (${jumps.join(' | ')})`,
          )
        if (!blindFrames && worstAccel > 8)
          fail(
            `the follow changed speed by ${Math.round(worstAccel)}px/frame in one frame: that is a jolt, not a glide`,
          )
        if (!blindFrames && worstGap > 220)
          fail(`the newest line sat ${Math.round(worstGap)}px below the fold while streaming`)
        if (!blindFrames && away > 4) fail(`the view finished ${Math.round(away)}px short of the bottom`)
        // Reading position is respected: scrolling up stops the follow, and the
        // text a reader stopped on does not move when a block commits under them.
        const maxScroll = sc.scrollHeight - sc.clientHeight
        let readerMoved = Number.NaN
        if (blindFrames) {
          blindSkips++
          pageLog(
            `selftest note: skipped the reader-up half of this check; the frame clock never came back, and with no paints the transcript scrolls ${maxScroll}px`,
          )
        } else if (maxScroll < 120)
          fail(`the transcript only scrolls ${maxScroll}px: the reader-up half of this check needs more filler`)
        sc.scrollTop = Math.max(0, sc.scrollTop - 220)
        await new Promise((r) => setTimeout(r, 80))
        if (!blindFrames && sc.scrollTop > sc.scrollHeight - sc.clientHeight - 40)
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
        if (!anchor && !blindFrames) fail('no settled text to anchor the scrolled-up reader to')
        if (anchor) {
          const rng = anchor as Range
          const anchorTop = rng.getBoundingClientRect().top
          transcript?.apply({ kind: 'text', d: '\n\n## A committed heading ends the paragraph\n\n' })
          await frame(40)
          transcript?.flushNow()
          if (!rng.startContainer.isConnected)
            fail("the line a reader had stopped on was replaced when a block committed below it")
          readerMoved = rng.getBoundingClientRect().top - anchorTop
          if (Math.abs(readerMoved) > 1.5)
            fail(`committing a block moved the reader's line ${readerMoved.toFixed(1)}px`)
        }
        transcript?.apply({ kind: 'turnEnd', d: { turn_id: 3, finish: 'stop' } })
        await new Promise((r) => setTimeout(r, 40))
        transcript?.clear()
        pageLog(
          `stream follow ok (worst frame step ${Math.round(worstStep)}px, worst speed change ` +
            `${Math.round(worstAccel)}px/frame over ${frames} frames; worst ${Math.round(worstGap)}px below ` +
            `the fold, settled ${Math.round(away)}px off bottom, reader's line moved ${
              Number.isFinite(readerMoved) ? `${readerMoved.toFixed(1)}px` : 'was not measurable'
            } on commit)`,
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
        await invoke('set_reasoning', { level: 'off', persist: true })
        draft = 'Write about coffee roasting in exactly four paragraphs. Answer in the chat only, use no tools and create no files.'
        await send()
        const stop = startFrameCadence()
        const done = await waitTurnDone(cadBefore)
        const c = stop()
        await invoke('set_reasoning', { level: 'medium', persist: true })
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
        // "Starved" means the page could not be given a clock: either the machine is that busy, or
        // the sample itself is empty - a baseline of 0 frames answers nothing, and grading a streaming
        // sample against it is how one run reported "0 animation frames in 0ms" as a stalled page.
        const starved = idle.median > 21 || idle.frames < 8 || c.frames < 2 || c.ms < 200
        if (offScreen()) {
          blindSkips++
          // A throttled frame clock is what an occluded page does, and this run
          // already proved the page was not laid out (the geometry notes above). The
          // numbers stay in the log; they are just not treated as a verdict.
          pageLog(`selftest note: skipped the frame-cadence verdict; ${offScreen()}`)
        } else if (c.frames < 60 && !starved) {
          // A thin sample can mean two different things: the streaming path is costing frames, or the
          // compositor stopped handing them out. The page's own long-task observer tells them apart.
          // If the median gap is the idle clock's and the page ran no long task, the shortfall is one
          // hole the page was never inside, and calling that a stalled streaming path measures the
          // window rather than the code.
          const hole = c.worst
          if (c.median <= idle.median * 1.35 && c.longN === 0 && hole > 8 * idle.median) {
            blindSkips++
            pageLog(
              `selftest note: skipped the thin-sample claim; ${c.frames} frames in ${c.ms}ms, but the median gap ` +
                `(${c.median.toFixed(1)}ms) is the idle clock's, the page ran no long task, and a single ${Math.round(hole)}ms ` +
                'hole accounts for the shortfall: the compositor stopped delivering frames rather than the streaming path costing them',
            )
          } else
            fail(
              `frame cadence: only ${c.frames} animation frames in ${c.ms}ms while the page was `
              + `getting a ${idle.median.toFixed(1)}ms clock idle (${idle.frames} frames in the `
              + `baseline): a thin sample on a machine that can deliver frames is a stalled page, `
              + 'and the streaming path is the only thing running',
            )
        } else if (starved) {
          if (idle.frames < 8 || idle.median <= 0) {
            // A baseline of no frames answers nothing, and a ratio against it answers less: one run
            // reported a streaming median of 14.0ms against an idle of 0.0ms and called that a cost,
            // when the number only says the baseline sample was empty. Keep a gross absolute guard,
            // and say plainly that the comparison is unavailable.
            blindSkips++
            pageLog(
              `selftest note: the idle baseline delivered ${idle.frames} frame(s), so no relative cadence claim is available; ` +
                `the streaming sample stands alone (${c.frames} frames over ${c.ms}ms, median ${c.median.toFixed(1)}ms, ` +
                `p95 ${c.p95.toFixed(1)}ms, worst ${c.worst.toFixed(0)}ms, ${c.longN} long task(s))`,
            )
            if (c.median > 34 || c.longMs > 250)
              fail(
                `frame cadence: a ${c.median.toFixed(1)}ms median with ${Math.round(c.longMs)}ms of the page's own long tasks, and no idle baseline to compare against - that is slow on any machine`,
              )
          } else {
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
          }
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
      if (want('entrances', 'no entrance runs on content that was already on screen')) {
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
        transcript?.apply({ kind: 'turnStart', d: { turn_id: 91, model: 'selftest', session_id: 0 } })
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
          await frame(60)
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
        await frame(60)
        transcript?.apply({ kind: 'turnEnd', d: { turn_id: 91, finish: 'stop' } })
        await new Promise((r) => setTimeout(r, 150))

        // (a2) a page that reloaded mid-answer keeps the part that streamed before it
        //
        // The gateway hands a reloading client the open half of the turn, which is the only
        // copy of it that exists until the turn ends. The transcript has to put it on screen
        // and then let the live deltas carry on inside the same block: the failure this is
        // about is an answer that starts where the reload landed, top half gone, repairing
        // itself only when the turn finally closed.
        transcript?.apply({ kind: 'turnStart', d: { turn_id: 95, model: 'selftest', session_id: 0 } })
        transcript?.seedOpen('the half that arrived before this page reloaded', 'and the reasoning before it')
        transcript?.apply({ kind: 'text', d: ' — and the half that arrived after.' })
        await frame(80)
        transcript?.apply({ kind: 'turnEnd', d: { turn_id: 95, finish: 'stop' } })
        await new Promise((r) => setTimeout(r, 200))
        {
          const bodies = [...document.querySelectorAll('.blk-answer .blk-body')].map(
            (b) => b.textContent ?? '',
          )
          const whole = bodies.find((t) => t.includes('half that arrived before this page reloaded'))
          if (!whole) fail('a reloading page lost the half of the answer that streamed before it connected')
          else if (!whole.includes('and the half that arrived after'))
            fail('the seeded half is on screen but the live half did not continue it: one answer, two blocks')
          else
            pageLog(
              'mid-turn reload ok: the streamed half is painted and the live deltas continue the same block',
            )
        }

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
        const blindTraffic = !reduced && unpainted()
        if (blindTraffic) {
          // A page the compositor is not painting starts no animations at all, so "we watched the
          // traffic and saw none" cannot be told apart from "the animations are gone". Say which case
          // this is rather than failing a feature for the state of the window.
          blindSkips++
          pageLog(
            `selftest note: skipped the reveal-traffic claims (${tins} word reveals, ${rises} block entrances seen); ${unpainted()} - ` +
              'an un-composited page starts no animations, so a count of zero proves nothing either way',
          )
        } else if (!reduced && tins < 10) fail(`only ${tins} word reveals were seen: the replay watch proved nothing`)
        if (!blindTraffic && !reduced && rises < 3) fail(`only ${rises} block entrances were seen: the replay watch proved nothing`)
        if (!blindTraffic && !reduced && roundTrip && rises > 6)
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

      // --- a thought is one line; the answer keeps its outdented label ----------
      // This check used to measure the thinking body's alignment under its own label: label,
      // rule and body edge sharing an x. There is no thinking body any more, so what survives is
      // the half that was never about thinking - the answer hangs its label in the left gutter
      // while its text stays on the column edge - plus the line a thought does leave, which has
      // to stay one line, inside the run, with nothing under it.
      if (want('thought-line', 'a thought is one line; the answer keeps its outdented label')) {
        // The premise is verified BEFORE the blocks exist. Verifying it after capturing
        // the nodes is worse than not verifying it: the recovery reopens a session,
        // that reopen repaints the transcript, and the captured nodes are then measured
        // while detached - which is how "expanded thinking block has no width: 0" came
        // back three runs in a row with the view demonstrably open.
        if (await blind('thinking and answer alignment')) {
          // counted skip; blind() logged why
        } else {
        transcript?.clear()
        transcript?.apply({ kind: 'turnStart', d: { turn_id: 1, model: 'selftest', session_id: 0 } })
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
          // A thought is one line now: no body to open, no display setting that uncovers it, and
          // a header that has to stay a single line while carrying a duration and a word count.
          const head = think.querySelector('.blk-head') as HTMLElement
          // A zero-width line has a cause, and the cause is in the ancestor chain: the first
          // entry marked [display:none] is the thing that hid it. Without this the failure reads
          // as "CSS is broken" and costs three more runs to unpick.
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
          const run = think.closest('.blk-run') as HTMLElement | null
          if (run && !run.classList.contains('open')) {
            // A folded accordion has no layout: every entry inside it measures 0x0. The check takes
            // the gesture a reader takes and says so, rather than failing on a measurement of
            // nothing and sending the next reader off to hunt for broken CSS.
            ;(run.querySelector('.blk-head') as HTMLElement | null)?.click()
            await renderTick()
            if (!run.classList.contains('open'))
              fail('opening the run at its header left the thinking line with no layout to measure')
          }
          const hr = head.getBoundingClientRect()
          if (hr.width < 60) {
            pageLog(`selftest diag: thinking line ${Math.round(hr.width)}px, chain=${chain(think)}`)
            fail(`the thinking line measures ${Math.round(hr.width)}px wide`)
          }
          if (hr.height > 22) fail(`the thinking line is ${Math.round(hr.height)}px tall, so it wrapped`)
          if (think.querySelectorAll('.blk-body').length)
            fail('the thinking line still carries a body element: a thought has nothing to open')
          if (!think.closest('.blk-run')) fail('the thinking line is not inside the run accordion')
          const aHead = document.querySelector('.turn-who-cwoffee .who-name')!.getBoundingClientRect()
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
        pageLog('a thought is one line and the answer keeps its outdented label')
        }
      }

      // --- the label does not crowd the answer while it streams -----------------
      // Reported as "the CWOFFEE tag sits too close to the text while it writes, and
      // it fixes itself at the end". The label lives in the gutter, so what the user
      // is describing is horizontal: the breathing room between the tag and the first
      // glyph. It has to be the same room while text arrives as after the markdown
      // pass, or the turn visibly shifts as it finishes.
      if (want('label-crowd', 'the label does not crowd the answer while it streams')) {
        transcript?.clear()
        transcript?.apply({ kind: 'turnStart', d: { turn_id: 1, model: 'selftest', session_id: 0 } })
        const gap = () => {
          const blk = document.querySelector('.blk-answer') as HTMLElement
          if (!blk) return null
          const head = document.querySelector('.turn-who-cwoffee .who-name')!.getBoundingClientRect()
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
          await frame(40)
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

      // --- one name per turn, pinned while that turn is on screen ---------------
      // Reported as: "both the You and the Cwoffee indicators have to appear only once per
      // turn — like the action buttons on a code block, float them until the end of the
      // container." A turn that printed its own name five times down the gutter is the same
      // complaint as an ungrouped transcript: five separate messages instead of one turn. The
      // divider travels with it: "the user message is above the turn divider" — your words open
      // the turn, so they belong under the line, not over it.
      if (want('sticky-name', 'one name per turn, pinned while that turn is on screen')) {
        transcript?.clear()
        transcript?.addUser('two things, please')
        const before = [...document.querySelectorAll('.turn')] as HTMLElement[]
        transcript?.apply({ kind: 'turnStart', d: { turn_id: 71, model: 'selftest', session_id: 0 } })
        const turns = [...document.querySelectorAll('.turn')] as HTMLElement[]
        const t = turns[turns.length - 1]
        const kids = [...t.children]
        const headAt = kids.findIndex((n) => n.classList.contains('turn-head'))
        const askAt = kids.findIndex((n) => n.classList.contains('blk-user'))
        if (headAt < 0) fail('this turn has no divider at all')
        else if (askAt < 0) fail('your message is not in the turn it opened')
        else if (headAt > askAt) fail('your message still sits above the turn divider')
        const stale = before.filter((x) => x !== t && x.querySelector('.blk-user'))
        if (stale.length) fail('your message stayed in the turn it was typed into as well')

        transcript?.apply({ kind: 'reasoning', d: 'check the sheet first' })
        transcript?.apply({ kind: 'toolCallBegin', d: { id: 'n1', name: 'read_file' } })
        transcript?.apply({ kind: 'toolExecEnd', d: { id: 'n1', ok: true, exit_code: 0, changes: [] } })
        transcript?.apply({ kind: 'text', d: 'First paragraph of the answer. ' })
        transcript?.apply({ kind: 'reasoning', d: 'and then the second half' })
        transcript?.apply({ kind: 'toolCallBegin', d: { id: 'n2', name: 'exec' } })
        transcript?.apply({ kind: 'toolExecEnd', d: { id: 'n2', ok: true, exit_code: 0, changes: [] } })
        transcript?.apply({ kind: 'text', d: 'Second paragraph, still the same turn.' })
        transcript?.flushNow()
        await frame(40)
        const paras = t.querySelectorAll('.blk-answer')
        const names = t.querySelectorAll('.turn-who-cwoffee')
        if (paras.length < 2) fail(`the fixture lost a paragraph (${paras.length})`)
        if (names.length !== 1)
          fail(`a turn with ${paras.length} paragraphs and an accordion names itself ${names.length} times, want once`)
        const loud = [...t.querySelectorAll('.blk-answer > .blk-head')].filter((n) => getComputedStyle(n).display !== 'none')
        if (loud.length) fail(`${loud.length} paragraphs still print their own CWOFFEE label`)
        // Two of your messages in one turn is one ask, so it keeps one name.
        transcript?.addUser('and one more thing')
        const asks = [...t.querySelectorAll('.blk-user')]
        if (asks.length !== 2) fail(`both of your messages should sit in this turn, got ${asks.length}`)
        const youNamed = asks.filter((n) => getComputedStyle(n.querySelector('.blk-head')!).display !== 'none')
        if (youNamed.length !== 1) fail(`${youNamed.length} of your messages in one turn are named, want one`)

        // Pinned for the length of the turn and released by its end — the affordance a code
        // block's buttons already have. A filler inside the turn makes it tall enough to have a
        // middle, and the turn itself is the shipped one rather than a hand-built shape.
        const sc = document.querySelector('.scroller') as HTMLElement | null
        const nmEl = t.querySelector('.turn-who .who-name') as HTMLElement | null
        if (!sc || !nmEl) fail('there is no scroller or no name marker to measure')
        else {
          const prevTop = sc.scrollTop
          const filler = document.createElement('div')
          filler.style.height = '1200px'
          t.append(filler)
          // And something after the turn: without it the turn is the last thing in the document
          // and "scrolled past its end" is physically unreachable - the scroll clamps and the
          // check reads the CSS as the thing that is broken.
          const tail = document.createElement('div')
          tail.style.height = '900px'
          sc.append(tail)
          // Wait for the page to stop moving under its own power before measuring it. The stream
          // follows the bottom of the transcript with an accelerating animation, and it marks its own
          // scrolls as programmatic for a frame - which swallows this check's scroll if the two
          // interleave, leaving the ratchet chasing a bottom the check just moved 2,000px away. A
          // name measured mid-chase is a measurement of the animation, not of the CSS.
          const settle = async () => {
            let last = -1
            let still = 0
            for (let i = 0; i < 24; i++) {
              await new Promise((r) => setTimeout(r, 30))
              const now = sc.scrollTop
              if (Math.abs(now - last) < 0.6 && ++still >= 2) break
              if (Math.abs(now - last) >= 0.6) still = 0
              last = now
            }
          }
          await settle()
          // Where the turn's top belongs once the view is 400px into it: 400px above the top of the
          // scroller, because scrolling down carries the top of the turn up past the line. Stated as
          // a target rather than as a scroll amount so the loop below can re-aim if the page moved.
          const want = sc.getBoundingClientRect().top - 400
          let scrolls = 0
          for (; scrolls < 3; scrolls++) {
            sc.scrollTop += t.getBoundingClientRect().top - want
            await settle()
            if (Math.abs(t.getBoundingClientRect().top - want) <= 8) break
          }
          const sr = sc.getBoundingClientRect()
          const stickAt = sr.top
          // The word hangs below the row it is pinned by, by the offset the CSS publishes for this
          // turn's shape, so "pinned at the top" has to be measured against that line rather than
          // against the row itself - otherwise a turn that opens with the folded group reads as a
          // name that has forgotten to stick.
          const hang = parseFloat(getComputedStyle(nmEl.parentElement!).getPropertyValue('--who-top')) || 0
          const mid = { nm: nmEl.getBoundingClientRect(), turn: t.getBoundingClientRect() }
          const landed = mid.turn.top - want
          const verdict: string[] = []
          if (Math.abs(landed) > 12)
            verdict.push(
              `this check could not site the turn 400px into the transcript - it landed ${Math.round(landed)}px off after ${scrolls} scroll${scrolls === 1 ? '' : 's'}`,
              // The scroll state, not just the answer: a correction that buys nothing every time
              // means the scroller refused the scroll (clamped, zero-height, or not the scroller at
              // all), and the numbers say which.
              `[scroller ${sc.tagName}.${sc.className.split(' ').slice(0, 3).join('.')} scrollTop=${Math.round(sc.scrollTop)}/`
                + `max=${Math.round(sc.scrollHeight - sc.clientHeight)} sh=${Math.round(sc.scrollHeight)} ch=${Math.round(sc.clientHeight)} `
                + `rect=${Math.round(sr.top)}..${Math.round(sr.bottom)} ovf=${getComputedStyle(sc).overflowY}; `
                + `turn connected=${t.isConnected} ${Math.round(t.getBoundingClientRect().top)}..${Math.round(t.getBoundingClientRect().bottom)} `
                + `offsetH=${t.offsetHeight} parent=${t.parentElement?.className ?? 'none'}; `
                + `want=${Math.round(want)} filler=${!!filler.parentElement} tail=${!!tail.parentElement}]`,
            )
          else if (mid.turn.top > stickAt)
            verdict.push(
              `the turn never reached the sticky line (still ${Math.round(mid.turn.top - stickAt)}px below it), so nothing can be said about its name`,
            )
          else if (offScreen() || (nmEl.parentElement as HTMLElement).getBoundingClientRect().height < 2) {
            // A row with no height is what an un-painted page answers; there is no sticky line to
            // measure against and the fixed offset is being compared with nothing.
            blindSkips++
            pageLog(
              `selftest note: skipped the mid-turn sticky-name measurement; ${offScreen() ?? 'the row the name hangs from measures 0px tall'} - a page nothing composites has no sticky line to measure against`,
            )
          } else if (Math.abs(mid.nm.top - (stickAt + hang)) > 3) {
            // The number alone cannot be acted on: the offset is the sum of where the row holding
            // the name landed, what the CSS variable says, and the label's own box padding. Report
            // all three, and what the turn opens with, so the next run names the layer at fault.
            const who = nmEl.parentElement as HTMLElement
            const wr = who.getBoundingClientRect()
            const cs = getComputedStyle(nmEl)
            verdict.push(
              `400px into a turn the name sits ${Math.round(mid.nm.top - stickAt)}px from the top of the transcript rather than ${hang}px; ` +
                `the row it hangs from ("${who.className}") lands ${Math.round(wr.top - stickAt)}px below the sticky line and is ${Math.round(wr.height)}px tall, ` +
                `the label's own top/padding are ${cs.top}/${cs.paddingTop}, ` +
                `and the turn opens with "${t.firstElementChild?.className || 'nothing'}"`,
            )
          }
          // What is actually beside the stuck word at this scroll position. The offset the CSS
          // publishes is arithmetic against the header of the block the turn opens with, and that is
          // true at the top of a turn. Four hundred pixels in, that header has scrolled away and the
          // same offset is measured against nothing - so measure the neighbour when it is there and
          // say plainly when it is not, rather than letting one constant answer both questions.
          if (Math.abs(landed) <= 12) {
            const heads = [...t.querySelectorAll('.blk-head')].filter(
              (n) => getComputedStyle(n).display !== 'none',
            ) as HTMLElement[]
            const beside = heads.find((n) => {
              const r = n.getBoundingClientRect()
              return r.bottom > stickAt && r.top < stickAt + 40
            })
            const rowOff = Math.round(nmEl.parentElement!.getBoundingClientRect().top - stickAt)
            pageLog(
              `stuck name 400px into a turn: ${Math.round(mid.nm.top - stickAt)}px below the sticky line (--who-top ${hang}px, the row it hangs from lands ${rowOff}px below the line); ` +
                (beside
                  ? `"${beside.textContent?.trim().slice(0, 22)}" is beside it, centres ${(mid.nm.top + mid.nm.height / 2 - (beside.getBoundingClientRect().top + beside.getBoundingClientRect().height / 2)).toFixed(1)}px apart`
                  : `none of the turn's ${heads.length} header(s) is beside it here, so the offset is measured against empty gutter`),
            )
          }
          sc.scrollTop += t.offsetHeight
          await settle()
          const after = { nm: nmEl.getBoundingClientRect(), turn: t.getBoundingClientRect() }
          if (after.turn.bottom > stickAt + 2)
            verdict.push(
              `the transcript would not scroll past the end of the turn (its bottom is still ${Math.round(after.turn.bottom - stickAt)}px below the line)`,
            )
          else if (after.nm.top > stickAt + hang + 2)
            verdict.push(`the name floated past the end of its own turn by ${Math.round(after.nm.bottom - after.turn.bottom)}px`)
          sc.scrollTop = prevTop
          filler.remove()
          tail.remove()
          if (verdict.length) fail(verdict.join('; '))
          else
            pageLog(
              `one name per turn, pinned for its length and released by its end, hanging ${hang}px off its row, turn sited ${Math.round(landed)}px off target after ${scrolls} scroll${scrolls === 1 ? '' : 's'} (and your message sits under the divider)`,
            )
        }
        // --- the transcript's marker: no dots, one cup at the bottom, name lined up with the header
        // Reported as "the dot animation and the assistant name are not aligned with the block
        // header", then as "remove the dot everywhere in the transcript, including beside the small
        // grouped actions inside the folded group", then as "put the two animated cups under the
        // name" - and that one came back wrong: "on multi small turns it gets duplicated and is quite
        // small; remove it from below the name and put it at the bottom of the content, below the
        // Copy | Share buttons, enlarged". So this asserts four things: nothing paints a generated
        // marker, the name shares the folded group's line, the page owns exactly one cup, and that cup
        // is the last thing in the content - moved there, never created twice.
        const whoRow = t.querySelector('.turn-who-cwoffee') as HTMLElement | null
        const cups = [...document.querySelectorAll('.transcript-cup')] as HTMLImageElement[]
        if (!whoRow) fail('this turn has no name row')
        else if (cups.length !== 1)
          fail(`${cups.length} cups on this page; one transcript gets one cup, however many turns it holds`)
        else {
          const cup = cups[0]
          const verdict: string[] = []
          const nm = whoRow.querySelector('.who-name') as HTMLElement | null
          if (!nm) verdict.push('the name row has no name in it')
          // (a) No dot on anything. The name row, each card and the grouped actions inside the
          //     folded group all used to grow one while a turn ran.
          const labels = [...t.querySelectorAll('.blk-head'), nm].filter(Boolean) as HTMLElement[]
          const dotted = labels.filter((n) => {
            const cs = getComputedStyle(n, '::before')
            return cs.content !== 'none' && cs.content !== 'normal'
          })
          if (dotted.length) verdict.push(`${dotted.length} labels still paint a generated ::before marker`)
          const anim = labels.filter((n) => getComputedStyle(n, '::before').animationName !== 'none')
          if (anim.length) verdict.push(`${anim.length} labels still animate a marker`)
          // (b) The cup: pinned in the band the pane reserves, big enough to read as the artwork
          //     rather than as a status pixel, and out of the way when the reader is mid-page.
          const host = cup.parentElement
          const nr = nm!.getBoundingClientRect()
          // The cup is only lit while the end of the transcript is on screen (see `updateCup`), so the
          // reading position is set before the picture is measured; a zero box from a hidden picture
          // would otherwise read as "the cup is 0px wide".
          const scE = document.querySelector('.scroller') as HTMLElement | null
          const prevE = scE?.scrollTop ?? 0
          if (scE) scE.scrollTop = scE.scrollHeight
          await new Promise((r) => setTimeout(r, 140))
          const ir = cup.getBoundingClientRect()
          if (!host || !host.classList.contains('scrollerwrap'))
            verdict.push(`the cup sits inside .${host?.className || 'nothing'} rather than in the band the pane reserves`)
          else if (host.lastElementChild !== cup)
            verdict.push(`the cup is not the last thing in its band (${host.lastElementChild?.className || 'nothing'} is)`)
          if (cup.closest('.transcript'))
            verdict.push('the cup is back inside the scrolling content, which is what made it travel')
          if (cup.classList.contains('bare')) verdict.push('the cup is hidden while the transcript has content')
          if (cup.classList.contains('cup-out'))
            verdict.push(`the cup went away at the end of a conversation (the pane is ${scE?.clientHeight}px showing ${scE?.scrollHeight}px of content)`)
          if (ir.width < 120) verdict.push(`the cup is ${Math.round(ir.width)}px wide; it should read as the drawing, not a pixel`)
          // (c) The name on the folded group's line. Both measured as *text* boxes: the row carries
          //     padding and the group carries margins, so comparing element boxes compares chrome.
          const centre = (n: Text | null, take: number) => {
            if (!n) return NaN
            const r = document.createRange()
            r.setStart(n, 0)
            r.setEnd(n, Math.min(take, n.length))
            const b = r.getBoundingClientRect()
            return b.top + b.height / 2
          }
          const runLabel = t.querySelector('.blk-run > .blk-head .run-label')
          const off = centre(runLabel?.firstChild as Text | null, 4) - centre(nm!.firstChild as Text | null, 4)
          const whoTop = getComputedStyle(whoRow).getPropertyValue('--who-top').trim()
          if (!runLabel) verdict.push('there is no folded-group header to align the name with')
          else if (Math.abs(off) > 2)
            verdict.push(`the name is ${off.toFixed(1)}px off the folded group's header line (--who-top ${whoTop || 'unset'})`)
          if (verdict.length) {
            // Geometry dump: a handful of these numbers is enough to say which element is lying,
            // and a bare "0px wide" says nothing about whether the layout, the CSS or the picture
            // is at fault.
            const cs = getComputedStyle(cup)
            const rowR = whoRow.getBoundingClientRect()
            const hostR = host?.getBoundingClientRect()
            const runR = t.querySelector('.blk-run')?.getBoundingClientRect()
            verdict.push(
              `[name ${Math.round(nr.top)}..${Math.round(nr.bottom)}; cup ${Math.round(ir.left)},${Math.round(ir.top)} `
                + `${Math.round(ir.width)}x${Math.round(ir.height)} complete=${cup.complete} natural=${cup.naturalWidth} `
                + `computed pos=${cs.position} disp=${cs.display} w=${cs.width} h=${cs.height} top=${cs.top} bottom=${cs.bottom} left=${cs.left}; `
                + `host .${host?.className || 'nothing'} ${hostR ? `${Math.round(hostR.left)},${Math.round(hostR.top)} ${Math.round(hostR.width)}x${Math.round(hostR.height)}` : 'none'}; `
                + `pane ${scE ? `${scE.scrollTop}/${scE.scrollHeight}x${scE.clientHeight} padB ${getComputedStyle(scE).paddingBottom}` : 'none'}; `
                + `view ${window.innerWidth}x${window.innerHeight} dpr ${window.devicePixelRatio}; `
                + `run ${runR ? `${Math.round(runR.left)},${Math.round(runR.top)} ${Math.round(runR.width)}x${Math.round(runR.height)}` : 'none'}`
                + `; header centre ${Number.isFinite(off) ? Math.round(centre(runLabel?.firstChild as Text | null, 4)) : 'n/a'}`
                + `; name centre ${Math.round(centre(nm?.firstChild as Text | null, 4))}]`,
            )
            fail(verdict.join('; '))
          } else
            pageLog(
              `turn marker ok: no generated marker on ${labels.length} labels, one cup ${Math.round(ir.width)}px pinned in the pane's band, `
                + `name ${off.toFixed(1)}px off the header line (--who-top ${whoTop})`,
            )

          if (scE) scE.scrollTop = prevE

          // Which drawing, and does the resting one carry a random hold? While a block works the cup
          // is the generating loop at the artist's own timing; once the turn stops it is the resting
          // loop, re-armed every lap with a hold drawn from IDLE_HOLD. Reading the hold back out of
          // the bytes the <img> actually points at is the only claim about a pause worth making.
          const busySrc = cup.getAttribute('src') ?? ''
          if (!busySrc.endsWith('/cwoffee_generating_generation_1.webp'))
            fail(`a working turn should show the generating cup, got ${busySrc}`)
          transcript?.apply({ kind: 'turnEnd', d: { turn_id: 71, finish: 'stop' } })
          await new Promise((r) => setTimeout(r, 120))
          // Turn end puts the buttons under the turn. The band is what keeps them clear of the cup:
          // at the end of a conversation the last row of content stops above the band, so the picture
          // cannot sit on top of the buttons.
          const acts = t.querySelector('.turn-acts')
          if (!acts) fail('the turn lost its Copy / Share / Branch row, so there is nothing to site the cup under')
          else {
            const scF = document.querySelector('.scroller') as HTMLElement | null
            if (scF) scF.scrollTop = scF.scrollHeight
            await new Promise((r) => setTimeout(r, 160))
            const ab = acts.getBoundingClientRect()
            const cr = cup.getBoundingClientRect()
            if (cup.classList.contains('cup-out'))
              fail(`the cup is put away at the end of a conversation (the pane shows ${scF?.clientHeight}px of ${scF?.scrollHeight}px)`)
            else if (cr.top < ab.bottom)
              fail(`the band does not clear the turn's buttons (buttons bottom ${Math.round(ab.bottom)}, cup top ${Math.round(cr.top)})`)
          }
          const idleSrc = cup.getAttribute('src') ?? ''
          if (!idleSrc.endsWith('/cwoffee_generating_idle_1.webp') && !idleSrc.startsWith('blob:'))
            fail(`a settled transcript should show the resting cup, got ${idleSrc}`)
          // Short turns must not multiply it. The cup is moved rather than created, so three more
          // one-line turns still leave exactly one on the page, still the last thing in the content.
          for (const id of [72, 73, 74]) {
            transcript?.apply({ kind: 'turnStart', d: { turn_id: id, model: 'selftest', session_id: 0 } })
            transcript?.apply({ kind: 'text', d: `short turn ${id}` })
            transcript?.apply({ kind: 'turnEnd', d: { turn_id: id, finish: 'stop' } })
          }
          await frame(40)
          const many = [...document.querySelectorAll('.transcript-cup')]
          const nturns = document.querySelectorAll('.turn').length
          if (many.length !== 1)
            fail(`${many.length} cups after ${nturns} turns; the cup has to move to the bottom, not spawn one per turn`)
          else if (cup.parentElement && cup.parentElement.lastElementChild !== cup)
            fail(`after ${nturns} turns the cup is no longer the last thing in the transcript`)
          else pageLog(`one cup across ${nturns} turns, still last in the content and still under the buttons`)
          await new Promise((r) => setTimeout(r, 1200)) // the picture's bytes, then the first re-arm
          const armed = cup.getAttribute('src') ?? ''
          if (!armed.startsWith('blob:'))
            fail(`the resting cup was never re-armed with a random hold; src is still ${armed}`)
          else {
            const buf = new Uint8Array(await (await fetch(armed)).arrayBuffer())
            const fr = aniFrames(buf)
            if (!fr) fail('the re-armed cup cannot be read as an animation this app patches')
            else if (fr.lastMs < IDLE_HOLD[0] || fr.lastMs > IDLE_HOLD[1])
              fail(`the resting cup's hold is ${fr.lastMs}ms, outside the ${IDLE_HOLD[0]}-${IDLE_HOLD[1]}ms range it draws from`)
            else
              pageLog(
                `resting cup re-armed: lap ${(fr.playMs / 1000).toFixed(2)}s carrying a ${fr.lastMs}ms hold, `
                  + `drawn fresh every lap, on the one cup the page owns`,
              )
          }
        }
        transcript?.clear()
      }

      // --- the view follows what was said, not what was worked, and one button gets back down --
      // Reported as: "now that the transcript only grows when actual message content comes through,
      // unlock the scroll from the bottom - but when a message comes through, the normal lock
      // applies again. If I scrolled up and stayed there before the message content begins to grow,
      // don't force a scroll; let me get locked when I scroll to the bottom. And maybe add a bottom
      // sticky centred button to scroll to the bottom." Five claims, one page: work does not move
      // the view; an answer does, for a reader who never left; a reader who did leave is not moved;
      // the button appears for them and not for anyone already at the bottom; and taking it
      // re-arms the lock. Plus the claim the removal of the preview created: a thinking run has to
      // say it is producing, in words, with nothing to watch.
      if (want('follow', 'the view follows what was said, not what was worked, and one button gets back ')) {
        if (await blind('follow policy and the jump button')) {
          // counted skip; blind() logged why
        } else {
          const sc = document.querySelector('.scroller') as HTMLElement | null
          const jump = () => document.querySelector('.jump') as HTMLElement | null
          const away = () => Math.round(sc!.scrollHeight - sc!.clientHeight - sc!.scrollTop)
          const verdict: string[] = []
          if (!sc) fail('there is no scroller to measure the follow policy in')
          else {
            // 0. a thought with no preview still has to look alive: the folded group's header
            //    counts the words as they land, which is also the stall detector a reader can see.
            transcript?.setVerbose(true)
            transcript?.clear()
            transcript?.apply({ kind: 'turnStart', d: { turn_id: 81, model: 'selftest', session_id: 0 } })
            transcript?.apply({ kind: 'reasoning', d: 'one thought and then another and another '.repeat(30) })
            await frame(60)
            const cnt = document.querySelector('.blk-run .run-count')?.textContent ?? ''
            if (!/\bwords\b/.test(cnt))
              verdict.push(`a run that is thinking says "${cnt}" where it should count the words arriving`)
            const line = document.querySelector('.blk-reasoning .blk-head')?.textContent ?? ''
            if (!/thinking/.test(line)) verdict.push(`a streaming thought reads "${line}" rather than counting itself`)
            // 1. fourteen thoughts and tool cards: a lot of height, and a view that holds still.
            for (let i = 0; i < 14; i++) {
              transcript?.apply({ kind: 'reasoning', d: `thinking about step ${i} of the run ` })
              transcript?.apply({ kind: 'toolCallBegin', d: { id: `w${i}`, name: 'shell' } })
              transcript?.apply({ kind: 'toolExecOutput', d: { id: `w${i}`, stream: 'stdout', chunk: `line ${i}: ${'x'.repeat(120)}\n` } })
              transcript?.apply({ kind: 'toolExecEnd', d: { id: `w${i}`, ok: true, exit_code: 0, changes: [] } })
            }
            await frame(60)
            const grew = away()
            if (grew < 120)
              verdict.push(`fourteen tool cards grew the content only ${grew}px below the fold: the page is too small to test this on`)
            if (sc.scrollTop !== 0)
              verdict.push(`the view moved ${sc.scrollTop}px during a turn of pure work; only what the model says may move it`)
            const jb = jump()
            if (!jb) verdict.push('nothing offers the reader a way back down while the newest content is below the fold')
            else {
              // 2. the button: sticky to the bottom of the transcript, centred on the column the
              //    text actually occupies (the gutter makes the pane and the column differ by 26px).
              const jr = jb.getBoundingClientRect()
              const sr = sc.getBoundingClientRect()
              const axis = sr.left + sr.width / 2 + 26
              if (getComputedStyle(jb).position !== 'absolute') verdict.push('the jump button is not pinned to the transcript')
              if (Math.abs(jr.left + jr.width / 2 - axis) > 6)
                verdict.push(`the jump button is ${Math.round(jr.left + jr.width / 2 - axis)}px off the text column's centre`)
              if (jr.bottom > sr.bottom - 2 || jr.bottom < sr.top + 20)
                verdict.push(
                  `the jump button is not at the bottom of the transcript (its bottom ${Math.round(jr.bottom)}, pane ${Math.round(sr.top)}..${Math.round(sr.bottom)})`,
                )
              if (jr.height < 22) verdict.push(`the jump button is ${Math.round(jr.height)}px tall, too small to hit with a thumb`)
              // The pill and the picture live in the same corner of the pane. The picture is put away
              // on the same gesture that raises the pill, so "both lit" is a bug rather than a
              // near-miss - and if both are lit, they must at least not be painted on each other.
              const cupEl = document.querySelector('.transcript-cup') as HTMLElement | null
              // A faded cup is not in the way, so "lit" here means legible, not merely painted.
              const cupLit =
                !!cupEl && !cupEl.classList.contains('bare') && parseFloat(getComputedStyle(cupEl).opacity) > 0.5
              if (cupLit) {
                const cr2 = cupEl.getBoundingClientRect()
                const ox = Math.min(jr.right, cr2.right) - Math.max(jr.left, cr2.left)
                const oy = Math.min(jr.bottom, cr2.bottom) - Math.max(jr.top, cr2.top)
                if (ox > 2 && oy > 2)
                  verdict.push(
                    `the jump button is painted on top of the cup's picture (${Math.round(ox)}x${Math.round(oy)}px of overlap) while the reader is ${away()}px above the bottom`,
                  )
              }
            }
            // 3. the model starts talking: for a reader who never left, the lock applies again and
            //    the newest line ends up on screen. The follow moves at a capped speed on purpose
            //    (that cap is what stopped the lurching), so what is asserted is that it converges,
            //    not that it caught a burst within one paint - the fixture lands a whole answer in
            //    one synchronous call, which is harsher than any stream on this machine.
            transcript?.apply({ kind: 'text', d: 'Here is what the run found. '.repeat(60) })
            let settledAt = -1
            for (let t = 0; t < 3000; t += 100) {
              await new Promise((r) => setTimeout(r, 100))
              if (away() <= 80) {
                settledAt = t
                break
              }
            }
            const gap = away()
            if (gap > 80)
              verdict.push(`a pinned reader was still ${gap}px above the answer after 3s of catch-up (grew ${grew}px)`)
            if (jump())
              verdict.push(`the jump button is still offering a way down to a reader ${gap}px from the bottom`)
            // 4. a reader who scrolled up is not moved by a model that keeps writing.
            sc.scrollTop = 0
            await new Promise((r) => setTimeout(r, 400))
            const marker = document.querySelector('.blk-answer .blk-body') as HTMLElement | null
            if (!marker) verdict.push('no answer text to mark a scrolled-up reader with')
            else {
              const before = marker.getBoundingClientRect().top
              transcript?.apply({ kind: 'text', d: ' And it keeps writing, which grows the page further below. '.repeat(40) })
              await new Promise((r) => setTimeout(r, 600))
              const moved = marker.getBoundingClientRect().top - before
              if (Math.abs(moved) > 1.5)
                verdict.push(`a reader who scrolled up was dragged ${Math.round(-moved)}px by streaming text`)
              if (!jump()) verdict.push('the reader who scrolled up lost the way back down')
              // 5. taking the button puts them at the bottom and re-arms the lock.
              jump()!.click()
              await new Promise((r) => setTimeout(r, 250))
              const back = away()
              if (back > 8) verdict.push(`the jump button left the reader ${back}px above the bottom`)
              if (jump()) verdict.push('the jump button stayed once the reader had taken it')
              transcript?.apply({ kind: 'text', d: ' One more line, which they should see without asking for it. ' })
              await new Promise((r) => setTimeout(r, 500))
              if (away() > 80) verdict.push(`after the jump the lock did not re-arm: ${away()}px above the newest line`)
            }
            // 6. your own message always lands at the bottom, even for a reader who had wandered off.
            sc.scrollTop = 0
            await new Promise((r) => setTimeout(r, 200))
            transcript?.addUser('and one more thing')
            await new Promise((r) => setTimeout(r, 250))
            const mine = away()
            if (mine > 8) verdict.push(`your own message did not bring the view to the bottom (${mine}px above it)`)
            if (verdict.length && unpainted()) {
              // The claims are kept in the log so the run still says what it could not prove; what it
              // does not do is grade a follow policy on a page that was given no frames.
              blindSkips++
              pageLog(
                `selftest note: skipped the follow-policy verdict; ${unpainted()}. It would have said: ${verdict.join('; ')}`,
              )
            } else if (verdict.length) {
              verdict.push(
                `[scroller top=${Math.round(sc.scrollTop)} max=${Math.round(sc.scrollHeight - sc.clientHeight)} sh=${Math.round(sc.scrollHeight)} ch=${Math.round(sc.clientHeight)}; jump=${jump() ? 'shown' : 'hidden'}; catch-up settled at ${settledAt < 0 ? 'never' : `${settledAt}ms`}]`,
              )
              fail(verdict.join('; '))
            } else
              pageLog(
                `follow ok: a turn of work left the view still and put a button at the bottom; the answer pulled a pinned reader back to the bottom in ${settledAt}ms; ` +
                  'a reader who scrolled up was not moved and one press put them back and re-armed the lock',
              )
            transcript?.setVerbose(false)
            transcript?.clear()
          }
        }
      }

      // --- the loop detector, run against the shipped copy -------------------------------------
      // `scripts/loop-probe.mjs` holds the same algorithm and the measurements behind the numbers.
      // This is the drift guard: if the two copies stop agreeing, the app's own test says so. The
      // reason it is worth a test at all is that the thinking preview is gone, so this is the only
      // thing left that can tell a reader the model is going round in circles.
      if (want('loop', 'the loop detector, run against the shipped copy')) {
        const SENT = 'The quarterly figures were restated after the auditor raised a concern about inventory. '
        const verdict: string[] = []
        const run = (text: string, reps: number) => {
          const w = new LoopWatch()
          const t0 = performance.now()
          for (let i = 0; i < reps; i++) w.push(text)
          return { s: w.stats(), ms: performance.now() - t0 }
        }
        // The loop shapes, each with how long it is allowed to take to notice.
        const a = run(SENT, 200) // 2,600 words of one sentence, over and over
        if (!a.s.fired || a.s.fired.kind !== 'phrase') verdict.push(`one sentence 200 times: ${JSON.stringify(a.s)}`)
        else if (a.s.fired.words > 80) verdict.push(`one sentence 200 times noticed ${a.s.fired.words} words in, allowed 80`)
        const b = run('so I should read the file and then the file and then the file and then ', 400)
        if (b.s.fired?.kind !== 'phrase') verdict.push(`a stalled thought: ${JSON.stringify(b.s)}`)
        const c = run(
          Array.from({ length: 400 }, (_, i) => `I should read the file notes/step.md and check line ${i + 1} for the value, then report the value. `).join(''),
          1,
        )
        if (!c.s.fired || c.s.fired.kind !== 'sentence')
          verdict.push(`the same sentence with one number changed: ${JSON.stringify(c.s)}`)
        const d = new LoopWatch()
        let dhit = null
        for (let i = 0; i < 40 && !dhit; i++) dhit = d.push('!'.repeat(400))
        if (!dhit || dhit.kind !== 'chars') verdict.push(`a wall of exclamation marks: ${JSON.stringify(d.stats())}`)
        // The controls: things a good answer does that are not a loop.
        const quiet: [string, { s: ReturnType<typeof run>['s'] }][] = [
          ['a finding restated three times', run('The margin came from inventory. ', 3)],
          ['a list quoted once', run('1. Open the notes file and read the first line. 2. Compare it with the totals from last quarter. 3. Write the summary in plain language. 4. Close the file and report the difference. ', 1)],
          ['a table with the same shape', run(Array.from({ length: 12 }, (_, i) => `| row ${i + 1} | grew | 1.${i + 1}% |`).join('\n'), 1)],
        ]
        for (const [name, r] of quiet)
          if (r.s.fired) verdict.push(`${name} was called a loop: ${describeLoop(r.s.fired)}`)
        // The cost, guarded in milliseconds rather than in adjectives: this runs inside the render
        // loop, on a machine that paints at 60Hz if it is lucky.
        const cost = run(SENT, 2000).ms // 26,000 words
        if (cost > 200) verdict.push(`the detector took ${cost.toFixed(0)}ms over 26,000 words, allowed 200`)
        if (verdict.length) fail(`loop detector: ${verdict.join('; ')}`)
        else
          pageLog(
            `loop ok: sentence-again seen in ${a.s.fired!.words} words, a stall in ${b.s.fired!.words}, a paraphrase loop in ` +
              `${c.s.fired!.words}, punctuation at delta ${dhit!.deltas}; three controls stayed quiet; ` +
              `26,000 words cost ${cost.toFixed(0)}ms (${(cost / 26000).toFixed(3)}ms per word)`,
          )
      }

      // --- a detected loop is visible where the reader is looking --------------------------------
      if (want('loop-visible', 'a detected loop is visible where the reader is looking')) {
        const before = fails
        transcript?.clear()
        transcript?.apply({ kind: 'turnStart', d: { turn_id: 82, model: 'selftest', session_id: 0 } })
        const toastBefore = document.querySelector('.toast')?.textContent ?? ''
        const SENT = 'The quarterly figures were restated after the auditor raised a concern about inventory. '
        for (let i = 0; i < 40; i++) transcript?.apply({ kind: 'reasoning', d: SENT })
        await frame(60)
        // The watch samples what arrived rather than every token, so the finding can land a beat
        // after the last delta - and on a page whose frame clock is silent, "a beat" is however long
        // the timeout floor says. Ask until it answers, then judge.
        let runc = document.querySelector('.blk-run.loops .run-count')
        for (let t = 0; t < 14 && !runc; t++) {
          await new Promise((r) => setTimeout(r, 100))
          runc = document.querySelector('.blk-run.loops .run-count')
        }
        const rc = (runc?.textContent ?? '').trim()
        if (!runc && unpainted()) {
          blindSkips++
          pageLog(
            `selftest note: skipped the run-header half of this check; ${unpainted()} - the watch samples on the frame clock, ` +
              'and a page that is not painting has no frame clock to sample on',
          )
        } else if (!runc) fail('a turn that repeated itself 40 times left no mark on the run header')
        else if (!/repeating/.test(rc)) fail(`the run header says ${JSON.stringify(rc)} where it should name the loop`)
        const again = document.querySelectorAll('.blk-run.loops').length
        if (again > 1) fail(`${again} run headers carry the loop mark; the finding is said once per turn`)
        // The other half of the finding: a note over the composer, because the app is the one
        // speaking here, not the model. Before this check the composer may already have been
        // showing a note, so what is asserted is that the note now up is the loop's.
        const toastNow = document.querySelector('.toast')?.textContent ?? ''
        if (!/repeating|loop/i.test(toastNow))
          fail(`a detected loop raised no note over the composer (the note showing is ${JSON.stringify(toastNow || 'none')}; the one before was ${JSON.stringify(toastBefore || 'none')})`)
        // A turn that is only working normally must not carry the mark.
        transcript?.clear()
        transcript?.apply({ kind: 'turnStart', d: { turn_id: 83, model: 'selftest', session_id: 0 } })
        for (let i = 0; i < 30; i++) {
          transcript?.apply({ kind: 'reasoning', d: `reading file ${i} and noting the ${i} differences in the totals ` })
          transcript?.apply({ kind: 'toolCallBegin', d: { id: `L${i}`, name: 'read' } })
          transcript?.apply({ kind: 'toolExecEnd', d: { id: `L${i}`, ok: true, exit_code: 0, changes: [] } })
        }
        await frame(60)
        if (document.querySelector('.blk-run.loops'))
          fail('an ordinary turn of 30 different tool calls was marked as a loop')
        else if (fails === before)
          pageLog('loop visible: the header names the repetition once, one note is raised, and an ordinary turn is left alone')
        transcript?.clear()
      }

      // --- three writes of one file are one attachment -------------------------
      // Reported as: "the same file got attached three times, and the block is too big — the
      // border is a broken line, just imply it, it is inside the invisible folder .cws." The
      // host now records one write per path per turn; the display half is what a session written
      // before that fix still needs, so it is asserted on stored rows rather than live events.
      if (want('one-attachment', 'three writes of one file are one attachment')) {
        const sheet = {
          path: '.cws/assistant_attachments/ph_used_laptops_rtx3070_4060_class.xlsx',
          name: 'ph_used_laptops_rtx3070_4060_class.xlsx',
          mime: 'application/vnd.openxmlformats-officedocument.spreadsheetml.sheet',
          bytes: 11_736,
          kind: 'attachment' as const,
        }
        transcript?.loadHistory([
          {
            id: 90_100,
            session_id: 0,
            role: 'user',
            content: 'which laptops under 900 have an RTX 3070 or 4060?',
            reasoning: '',
            tool_call_id: null,
            tool_calls_json: '',
            reasoning_signature: '',
            reasoning_ms: 0,
            files: [],
            created_at: 0,
            attachments: [],
          },
          {
            id: 90_101,
            session_id: 0,
            role: 'assistant',
            content: 'the sheet is ready',
            reasoning: '',
            tool_call_id: null,
            tool_calls_json: '',
            reasoning_signature: '',
            reasoning_ms: 0,
            created_at: 0,
            attachments: [],
            files: [{ ...sheet, bytes: 10_094 }, { ...sheet, bytes: 10_678 }, sheet],
          },
        ])
        const units = [...document.querySelectorAll('.attachunit')] as HTMLElement[]
        if (units.length !== 1) fail(`${units.length} attachment units for one file, want one`)
        if (units.length) {
          const u = units[0]
          if (!u.title.includes('written 3 times')) fail(`the unit does not say three writes went into it: "${u.title}"`)
          const r = u.getBoundingClientRect()
          if (r.height > 40) fail(`the attachment block is ${Math.round(r.height)}px tall, want a single line`)
          const chip = u.querySelector('.filechip') as HTMLElement | null
          if (!chip) fail('the unit has no chip inside it')
          else if (parseFloat(getComputedStyle(chip).borderTopWidth) > 0)
            fail('the chip still draws its own box inside the unit')
          const cs = getComputedStyle(u)
          if (cs.borderTopStyle !== 'dashed') fail(`the unit's edge is ${cs.borderTopStyle}, want a dashed implied one`)
          if (!u.querySelector('.filechip.one-line')) fail('the attachment chip is still laid out on two lines')
          if (!u.querySelector('.attach-save')) fail('the unit lost its Save control')
          pageLog(`attachment unit ok (3 writes -> 1 unit, ${Math.round(r.height)}px tall, dashed edge, no box around the name)`)
        }
        // A loaded session gets the same single cup, resting: one element per page means the
        // "one moving picture at a time" cost argument no longer forces stills into the history view.
        // That rule is exactly what made a loaded session's cup look frozen, which is what the report
        // "the idle webp isn't animated" was seeing.
        const loadedCups = [...document.querySelectorAll('.transcript-cup')] as HTMLImageElement[]
        if (loadedCups.length !== 1) fail(`${loadedCups.length} cups in a loaded session; one page owns one cup`)
        else {
          const c = loadedCups[0]
          const src = c.getAttribute('src') ?? ''
          const host = c.parentElement
          if (src.endsWith('_still.webp')) fail(`a loaded session's cup is a still (${src}); the resting loop belongs at the bottom`)
          else if (!host?.classList.contains('scrollerwrap'))
            fail(
              `the transcript's cup hangs in "${host?.className || 'nothing'}" rather than pinned in the transcript pane, where the padding reserves its band`,
            )
          else
            pageLog(
              `transcript cup ok (one cup for ${document.querySelectorAll('.turn').length} stored turns, pinned in the band the pane reserves for it, ` +
                `showing ${src.startsWith('blob:') ? 'an armed resting loop' : 'the resting loop'}` +
                `${matchMedia('(prefers-reduced-motion: reduce)').matches ? ' as a still, per the motion setting' : ''})`,
            )
        }
        if (units.length) {
          // The hand-over line. An attachment belongs to the reader, not to the list of files the
          // turn touched, so it sits against the right edge of the text column rather than in the
          // strip under the prose. Measured against the column, not the pane: the pane has a gutter
          // and padding, and a line can be flush right inside padding while sitting off the text.
          const u0 = units[0]
          const line = u0.parentElement
          if (!line?.classList.contains('attachline'))
            fail(`the attachment sits in .${line?.className || 'nothing'} rather than on its own hand-over line`)
          else {
            const col = document.querySelector('.scroller > div > .transcript') as HTMLElement | null
            const lr = line.getBoundingClientRect()
            const ur = u0.getBoundingClientRect()
            if (col) {
              const cr = col.getBoundingClientRect()
              if (Math.abs(cr.right - lr.right) > 4)
                fail(`the hand-over line ends ${Math.round(cr.right - lr.right)}px off the right edge of the text column`)
            }
            if (ur.right > lr.right + 1) fail(`the unit hangs ${Math.round(ur.right - lr.right)}px past its own line`)
            if (getComputedStyle(line).justifyContent !== 'flex-end')
              fail(`the hand-over line is justified ${getComputedStyle(line).justifyContent}, not to the right`)
          }
          // The control's hover fill is what the reader actually sees. Painted flush against the
          // unit's dashed edge, it looks like a rectangle that does not fit inside its own row.
          const save = u0.querySelector('.attach-save') as HTMLElement | null
          if (save) {
            const cs = getComputedStyle(u0)
            const pr = parseFloat(cs.paddingRight) || 0
            const pb = parseFloat(cs.paddingBottom) || 0
            const sr = save.getBoundingClientRect()
            const ur2 = u0.getBoundingClientRect()
            if (pr < 2 || pb < 2)
              fail(`the unit has ${pr}px/${pb}px of inboard air, so no control inside it can avoid its own border`)
            else if (ur2.right - sr.right < pr - 1 || ur2.bottom - sr.bottom < pb - 1)
              fail(
                `the Save control sits ${(ur2.right - sr.right).toFixed(1)}px from the right edge and ${(ur2.bottom - sr.bottom).toFixed(1)}px from the bottom, ` +
                  `inside a unit holding ${pr}px/${pb}px of padding: its hover fill lands on the dashed border`,
              )
          }
        }
        transcript?.clear()
      }

      // --- an icon sits in the middle of its own button -----------------------
      // Reported as: "in mobile (or whenever the hamburger menu shows up), the hamburger menu icon
      // itself is not centered inside its own button". The rule that makes the button appear at phone
      // width sets `display: inline-flex`, which replaces the shared icon button's `display: grid;
      // place-items: center`; a flex container left to its own defaults puts its child at the start of
      // the main axis and stretches it on the cross one, which is how the glyph ended up in the top-left
      // corner of a 38px tap target. The drawer cannot be opened at desktop width, so the check borrows
      // the one thing the media query changes - the display mode - and measures the real button with
      // its real classes.
      if (want('icon-center', 'an icon sits in the middle of its own button')) {
        if (await blind('the hamburger glyph in its button')) {
          // counted skip; blind() logged why
        } else {
          const verdict: string[] = []
          const btn = document.querySelector('button.sidemenu') as HTMLElement | null
          if (!btn) fail('there is no sidebar toggle button on the page')
          else {
            const svg = btn.querySelector('svg') as HTMLElement | null
            if (!svg) fail('the sidebar toggle has no icon inside it')
            else {
              const prev = btn.getAttribute('style') ?? ''
              for (const [p, v] of [
                ['display', 'inline-flex'],
                ['width', '38px'],
                ['height', '38px'],
                ['visibility', 'hidden'],
              ])
                btn.style.setProperty(p, v, 'important')
              await frame(0)
              const b = btn.getBoundingClientRect()
              const g = svg.getBoundingClientRect()
              btn.setAttribute('style', prev)
              await frame(0)
              if (b.width < 30 || b.height < 30)
                verdict.push(`the button measures ${Math.round(b.width)}x${Math.round(b.height)}px, not the phone tap target it is at narrow width`)
              const dx = g.left + g.width / 2 - (b.left + b.width / 2)
              const dy = g.top + g.height / 2 - (b.top + b.height / 2)
              if (Math.abs(dx) > 1.5 || Math.abs(dy) > 1.5)
                verdict.push(`the glyph sits ${Math.round(Math.abs(dx))}px ${dx < 0 ? 'left' : 'right'} and ${Math.round(Math.abs(dy))}px ${dy < 0 ? 'above' : 'below'} the centre of its own button`)
              if (verdict.length) fail(`hamburger icon: ${verdict.join('; ')}`)
              else
                pageLog(
                  `hamburger icon ok (a ${Math.round(g.width)}px glyph in a ${Math.round(b.width)}px button: off centre by ${dx.toFixed(1)}px across, ${dy.toFixed(1)}px down)`,
                )
            }
          }
        }
      }

      // --- the cup does not travel while the model writes ---------------------
      // Reported as: "the animated webp in the transcript jumps too much when the assistant is
      // generating new lines; can we prevent that with sticky or something?" As the last element in
      // the content its place on screen was a function of everything that arrived: each line pushed
      // it down, each follow step pulled it back, and a turn that held the view while tool output
      // piled up below the fold slid it off the bottom and then brought it flying back.
      // `ui/perf/probe.ts` measures exactly that over a synthetic stream, and the run before the fix
      // read: 1,348px of travel, p95 261px in a single frame, six reversals of direction in three
      // turns. Pinned in a band the pane reserves, the number has to come out zero - and the band
      // has to be deep enough that the newest line of text never runs under the picture.
      if (want('cup', 'the cup does not travel while the model writes')) {
        if (await blind('the cup band')) {
          // counted skip; blind() logged why
        } else {
          const verdict: string[] = []
          const img = document.querySelector('.transcript-cup') as HTMLElement | null
          const wrap = document.querySelector('.scrollerwrap') as HTMLElement | null
          const sc = document.querySelector('.scroller') as HTMLElement | null
          if (!img || !wrap || !sc) {
            fail(`the cup band check found no ${!img ? 'cup' : !wrap ? 'scrollerwrap' : 'scroller'}`)
          } else {
            transcript?.clear()
            await frame(0)
            // An empty transcript hides its cup on purpose - the opening screen has its own, larger
            // one - so the seat is measured with content on the page and the pane at the end of it.
            // Measuring a hidden cup measures a zero box at the page origin, which is how this check
            // once reported a picture 524px off the pane's bottom edge and 350px out of the column.
            transcript?.apply({ kind: 'turnStart', d: { turn_id: 91, model: 'selftest', session_id: 0 } })
            transcript?.addUser('the picture has to stay put while you write')
            for (let i = 0; i < 6; i++) {
              transcript?.apply({ kind: 'text', d: `warm-up line ${i}: the picture stays put while the model writes\n` })
              await frame(0)
            }
            sc.scrollTop = sc.scrollHeight
            await frame(2)
            const cs = getComputedStyle(img)
            const box = img.getBoundingClientRect()
            const host = (img.parentElement ?? wrap) as HTMLElement
            const wr = host.getBoundingClientRect()
            if (cs.position !== 'absolute') verdict.push(`the cup is ${cs.position}, so it scrolls with the content`)
            if (img.parentElement !== wrap)
              verdict.push(`the cup is pinned to .${host.className || 'nothing'} rather than to the transcript pane`)
            const seat = Math.round(wr.bottom - box.bottom)
            if (Math.abs(seat - 8) > 2)
              verdict.push(`the cup sits ${seat}px off the pane's bottom edge, not the 8px the band reserves`)
            const padB = parseFloat(getComputedStyle(sc).paddingBottom)
            if (!(padB >= box.height + 24))
              verdict.push(
                `the scroller reserves ${padB.toFixed(0)}px at the bottom for a ${box.height.toFixed(0)}px picture, so the newest line runs under it`,
              )
            const col = document.querySelector('.scroller > div > .transcript') as HTMLElement | null
            if (col) {
              const off = Math.round(box.left - col.getBoundingClientRect().left)
              if (Math.abs(off) > 2) verdict.push(`the cup sits ${off}px off the left edge of the text column`)
            }
            // Now keep streaming, and watch it not move. A reversal is what the eye reads as jitter,
            // which is why the count is reported even when the total is zero.
            let prev = img.getBoundingClientRect().top
            let travel = 0
            let worst = 0
            let reversals = 0
            let dir = 0
            let frames = 0
            let lit = 0
            let unlit = 0
            let toggles = 0
            let wasLit = !img.classList.contains('cup-out')
            for (let i = 0; i < 60; i++) {
              transcript?.apply({ kind: 'text', d: `line ${i}: the picture stays put while the model writes\n` })
              await frame(0)
              const box2 = img.getBoundingClientRect()
              const isLit = !img.classList.contains('cup-out') && box2.height > 2
              if (isLit !== wasLit) {
                toggles++
                wasLit = isLit
              }
              if (isLit) lit++
              else unlit++
              // A hidden picture has no box: measuring its top would report the 136px it travelled
              // coming back as if the pinned drawing had jumped.
              if (isLit) {
                const d = box2.top - prev
                if (Math.abs(d) > 0.5) {
                  travel += Math.abs(d)
                  worst = Math.max(worst, Math.abs(d))
                  const nd = Math.sign(d)
                  if (dir !== 0 && nd !== dir) reversals++
                  dir = nd
                }
                prev = box2.top
              }
              frames++
            }
            transcript?.apply({ kind: 'turnEnd', d: { turn_id: 91, finish: 'stop' } })
            await frame(4)
            // ...and it is put away when the reader is in the middle of the page. A picture pinned to
            // the pane would otherwise be painted over other people's paragraphs.
            const range = sc.scrollHeight - sc.clientHeight
            // 60px is 2.5 lines: enough above the end that the band - and therefore the picture's
            // seat - is off screen, and the transcript puts the drawing away rather than painting it
            // across the paragraph the reader stopped on.
            const pullBack = 60
            const mid = range > pullBack ? range - pullBack : 0
            sc.scrollTop = mid
            // The fade is real, so one sample right after the move catches the picture mid-flight and
            // reads like a failure. Poll until it settles; the opacity claimed below is then the
            // settled one.
            let dimSettled = false
            for (let t = 0; t < 25; t++) {
              await new Promise((r) => setTimeout(r, 80))
              if (img.classList.contains('cup-out') && parseFloat(getComputedStyle(img).opacity) <= 0.16) {
                dimSettled = true
                break
              }
            }
            const dimmedAway = dimSettled
            const midBox = img.getBoundingClientRect()
            const midOpacity = parseFloat(getComputedStyle(img).opacity)
            sc.scrollTop = sc.scrollHeight
            await frame(3)
            // The way back is driven by a scroll event, which can land late; poll for it rather than
            // grading the first frame after the write.
            let back = !img.classList.contains('cup-out')
            for (let t = 0; t < 12 && !back; t++) {
              await new Promise((r) => setTimeout(r, 100))
              back = !img.classList.contains('cup-out')
            }
            // Then ride the fade back up: the class flips first and the opacity follows. The first
            // in-between value seen is the fade itself, and it is reported rather than demanded -
            // whether a frame lands in that 420ms window is the compositor's business.
            let midFade = Number.NaN
            for (let t = 0; t < 20; t++) {
              const o = parseFloat(getComputedStyle(img).opacity)
              if (back && o > 0.16 && o < 0.9 && !Number.isFinite(midFade)) midFade = o
              if (back && o > 0.9) break
              await new Promise((r) => setTimeout(r, 80))
            }
            const backOpacity = parseFloat(getComputedStyle(img).opacity)
            const backAgain = back && backOpacity > 0.9
            // The loop above stops at the first frame past 0.9; the number worth printing is the one
            // the fade ends at, so let it land before reading it. "back to 0.90" would otherwise read
            // like a picture that never quite came back.
            let backSettled = backOpacity
            if (back) {
              await new Promise((r) => setTimeout(r, 520))
              backSettled = parseFloat(getComputedStyle(img).opacity)
            }
            const cupBlind = unpainted()
            if (range <= pullBack)
              pageLog(
                `selftest note: the cup-away half of this check could not be measured; the transcript offered only ${range}px of scroll, so no reading position on this page is ` +
                  'far enough above the end for the picture to be put away',
              )
            else if (cupBlind) {
              // The put-away is driven by the scroller's own `scroll` event, and an un-composited page
              // does not dispatch one: writing scrollTop moves the position but nothing observes it.
              // That is indistinguishable from the feature being gone, so it is reported as a blind
              // spot rather than graded.
              blindSkips++
              pageLog(`selftest note: skipped the cup-away half of this check; ${cupBlind} - and no scroll event is dispatched without painting`)
            } else if (!dimmedAway)
              verdict.push(
                `the cup stayed at opacity ${getComputedStyle(img).opacity} ${Math.round(sc.scrollHeight - sc.clientHeight - mid)}px above the end of the transcript - a 136px drawing painted over the middle of a paragraph`,
              )
            // It fades, it does not go: the picture keeps its box, so the transition has somewhere to
            // run from and to, and a reader who scrolls back finds the same drawing instead of one
            // that appeared out of nothing.
            if (!cupBlind && midBox.height < 100)
              verdict.push(
                `the cup lost its layout mid-page (${Math.round(midBox.height)}px tall): a dimmed picture keeps its box, only a hidden one drops it`,
              )
            if (!cupBlind && midOpacity > 0.16)
              verdict.push(`the cup is still ${midOpacity.toFixed(2)} opaque mid-page: the prose under a 136px drawing has to stay readable`)
            if (!cupBlind && !matchMedia('(prefers-reduced-motion: reduce)').matches && !/opacity/.test(getComputedStyle(img).transitionProperty))
              verdict.push(
                `the cup's transitionProperty is "${getComputedStyle(img).transitionProperty}": the fade has to be declared on the base rule, or it runs one way and snaps the other`,
              )
            if (!cupBlind && !backAgain) verdict.push('the cup did not come back when the reader went back to the end')
            if (!cupBlind && travel > 2)
              verdict.push(`the pinned cup still moved ${travel.toFixed(1)}px while 60 lines streamed (worst frame ${worst.toFixed(1)}px)`)
            if (!cupBlind && frames < 40) verdict.push(`the streaming half sampled only ${frames} frames and measured nothing`)
            // A cup that blinks off and on during a turn is worse than one that stays put: the
            // follow's own catch-up leaves the view a few dozen px short of the bottom for a frame
            // at a time, and a 136px picture that answers that by disappearing is a flicker.
            if (!cupBlind && toggles > 2)
              verdict.push(
                `the cup lit and put itself away ${toggles} times during 60 streamed lines (${lit} lit frame(s), ${unlit} away) - the hide has no patience, so it flickers with the follow`,
              )
            if (verdict.length) {
              // Every number here distinguishes one cause from the next: a cup at the wrong seat is a
              // CSS problem, a cup whose parent is not the pane is a placement problem, and a parent
              // that is shorter than the pane means the pane itself is not the box being measured.
              const wr2 = host.getBoundingClientRect()
              const sr = sc.getBoundingClientRect()
              fail(
                `cup band: ${verdict.join('; ')} [cup ${Math.round(box.left)},${Math.round(box.top)} ${Math.round(box.width)}x${Math.round(box.height)} ` +
                  `computed pos=${cs.position} disp=${cs.display} top=${cs.top} bottom=${cs.bottom} left=${cs.left} w=${cs.width}; ` +
                  `parent .${host.className || 'nothing'} ${Math.round(wr2.left)},${Math.round(wr2.top)} ${Math.round(wr2.width)}x${Math.round(wr2.height)}; ` +
                  `scroller ${Math.round(sr.left)},${Math.round(sr.top)} ${Math.round(sr.width)}x${Math.round(sr.height)} ` +
                  `${sc.scrollTop}/${sc.scrollHeight}x${sc.clientHeight} padB ${getComputedStyle(sc).paddingBottom}; ` +
                  `column ${col ? `${Math.round(col.getBoundingClientRect().left)}..${Math.round(col.getBoundingClientRect().right)}` : 'none'}; ` +
                  `chain ${[img, host, sc, sc.parentElement, sc.parentElement?.parentElement].filter(Boolean).map((n) => `.${(n as HTMLElement).className || 'nothing'}`).join(' < ')}]`,
              )
            } else
              pageLog(
                `cup band ok (${Math.round(box.height)}px picture pinned ${seat}px off the pane's bottom, ${padB.toFixed(0)}px of the pane held out of the text, ` +
                  `column-aligned; across ${frames} streaming frames it travelled ${travel.toFixed(1)}px, worst frame ${worst.toFixed(1)}px, ${reversals} reversals, ` +
                  `${toggles} lit/away toggle(s) in ${lit} lit and ${unlit} put-away frames; faded to ${midOpacity.toFixed(2)} mid-page and back to ${backSettled.toFixed(2)} once the fade landed${
                    Number.isFinite(midFade)
                      ? `, caught mid-fade at ${midFade.toFixed(2)} on the way back in`
                      : ', no mid-fade frame caught (the compositor clock, not a claim either way)'
                  }; both ways by transition)`,
              )
            transcript?.clear()
          }
        }
      }

      // --- the idle cup is one picture and one motion ------------------------
      // The opening screen's cup is now an animated WebP whose frame table ends with a
      // five-second hold. Two ways this goes wrong silently. One: CSS animates a picture that
      // already animates, stretching the same artwork twice - exactly what the old
      // `cwoffee-breathe` would do to it. Two: the still that reduced motion needs is not in
      // the bundle. A media query can stop a keyframe but it cannot pause a raster loop, so
      // reduced motion has to be a different file - and a missing file in a desktop app is a
      // silent 404. Both are checkable against the shipped asset and the shipped rules. The
      // hold itself was measured in this engine by snapshotting a webview on a timer: the
      // picture's pixels stay quiet for 5.05s between cycles.
      if (want('cup-art', 'the idle cup is one picture and one motion')) {
        const load = (src: string) =>
          new Promise<{ w: number; h: number }>((resolve) => {
            const img = new Image()
            img.onload = () => resolve({ w: img.naturalWidth, h: img.naturalHeight })
            img.onerror = () => resolve({ w: 0, h: 0 })
            img.src = src
          })
        const anim = await load('/cwoffee_initial_idle_1.webp')
        const still = await load('/cwoffee_idle_still.webp')
        if (anim.w < 400 || anim.h < 400) fail(`the idle cup does not decode from the bundle: ${JSON.stringify(anim)}`)
        if (still.w < 400 || still.h < 400) fail(`the reduced-motion still does not decode: ${JSON.stringify(still)}`)
        // The same four-file shape under each turn's name: a loop for working, a loop for resting,
        // and a still of each for reduced motion. A missing one of these is a silent 404 in a
        // desktop app, which is how "no cup appeared" gets reported with nothing in the log.
        for (const p of [
          '/cwoffee_generating_generation_1.webp',
          '/cwoffee_generating_idle_1.webp',
          '/cwoffee_generating_generation_still.webp',
          '/cwoffee_generating_idle_still.webp',
        ]) {
          const d = await load(p)
          if (d.w < 400 || d.h < 400) fail(`the turn's cup asset ${p} does not decode from the bundle: ${JSON.stringify(d)}`)
        }
        pageLog('turn cup assets ok (4 files in the bundle: two loops at the artist timing, two stills for reduced motion)')
        const seen: Record<string, string> = {}
        for (const sheet of Array.from(document.styleSheets)) {
          let rules: CSSRuleList
          try {
            rules = (sheet as CSSStyleSheet).cssRules
          } catch {
            continue // nothing here is cross-origin, but a sheet we cannot read must not throw
          }
          for (const r of Array.from(rules ?? [])) {
            const sel = (r as CSSStyleRule).selectorText
            if (sel) seen[sel] = r.cssText
          }
        }
        const cupRule = seen['.cupwrap .cup']
        if (!cupRule) fail('no `.cupwrap .cup` rule in the shipped CSS to read')
        else if (/animation(-name)?:\s*(?!none)/.test(cupRule)) fail(`CSS animates a picture that animates itself: ${cupRule}`)
        const boxW = parseFloat((seen['.cupwrap'] ?? '').match(/width:\s*([0-9.]+)px/)?.[1] ?? '0')
        // 300 of the new picture's 480 units are ink; the icon it replaced painted 224 of 256.
        // Below about 140px the cup would sit visibly smaller than the one it replaced.
        if (boxW < 140) fail(`the cup box is ${boxW}px wide; the new artwork would paint smaller than the icon it replaced`)
        // Nothing drawn over the cup. A cartoon wisp of steam used to be rendered here on top of
        // the picture: measured against the artwork it was anchored correctly and timed to the
        // cup's own cycle, and it still read as a second animation that had nothing to do with the
        // drawing. The picture carries the gesture, so the frame holds one element.
        const wrap = document.querySelector('.cupwrap')
        if (wrap && wrap.children.length !== 1)
          fail(`${wrap.children.length} elements sit inside .cupwrap; only the picture belongs there`)
        // Two cups on one page. The transcript keeps its own at the bottom of the conversation, and
        // an empty transcript sits under this screen - so the transcript's has to stand down here,
        // or the opening screen shows the same drawing twice at two sizes.
        const tCup = document.querySelector('.transcript-cup') as HTMLImageElement | null
        if (wrap && tCup && getComputedStyle(tCup).display !== 'none')
          fail('the opening screen and the transcript are showing the cup at the same time')
        if (wrap && tCup) pageLog('one cup on screen: the opening screen shows its own, the transcript keeps its copy hidden')
        // The wiring, not just the parts: when the opening screen is on show, the picture it
        // actually points at has to match what this session asked for. A media query cannot
        // pause a raster loop, so getting this wrong is not a style miss, it is motion the
        // user asked not to have. Once the random pause is running the picture is an armed copy
        // (a `blob:` of the shipped bytes with the last frame's duration rewritten); both that
        // and the shipped file are the moving cup, and a still is only ever correct when the
        // session asks for reduced motion.
        const cup = document.querySelector('.cupwrap .cup')
        let onShow = 'no opening-screen cup on this page to read'
        if (cup) {
          const wantsStill = matchMedia('(prefers-reduced-motion: reduce)').matches
          const src = cup.getAttribute('src') ?? ''
          if (wantsStill) {
            if (!src.endsWith('/cwoffee_idle_still.webp'))
              fail(`reduced motion has to be a still picture, but the cup on show is ${src}`)
            onShow = 'cwoffee_idle_still.webp, a still as the motion setting asks'
          } else if (src.startsWith('blob:')) onShow = 'an armed copy of the moving cup (random hold applied)'
          else if (src.endsWith('/cwoffee_initial_idle_1.webp')) onShow = 'cwoffee_initial_idle_1.webp (the shipped file, not yet re-armed)'
          else
            fail(`the opening screen is showing the wrong cup for this session's motion setting: `
              + `src=${src} while the media query says reduced motion is ${wantsStill}`)
        }
        pageLog(`idle cup ok (${anim.w}x${anim.h} animated picture + ${still.w}x${still.h} still for reduced motion, `
          + `${boxW}px box, nothing animating the picture in CSS, nothing drawn over it, ` + onShow + ')')
      }

      // --- a finished turn offers copy, share, and an honest branch button -----
      if (want('turn-buttons', 'a finished turn offers copy, share, and an honest branch button')) {
        transcript?.clear()
        transcript?.apply({ kind: 'turnStart', d: { turn_id: 1, model: 'selftest', session_id: 0 } })
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
        transcript?.apply({ kind: 'turnStart', d: { turn_id: 1, model: 'selftest', session_id: 0 } })
        transcript?.apply({ kind: 'text', d: 'downloaded the sources' })
        transcript?.addFiles([{ path: 'metr_report.html', name: 'metr_report.html', mime: 'text/html', bytes: 430006 }])
        transcript?.apply({ kind: 'turnEnd', d: { turn_id: 1, finish: 'stop' } })
        await new Promise((r) => setTimeout(r, 60))
        transcript?.apply({ kind: 'turnStart', d: { turn_id: 2, model: 'selftest', session_id: 0 } })
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

      // --- only the attachments folder gets a "save it where I like" control -----
      //
      // The report this pins: a turn's strip was full of files, any of which could be
      // clicked open from the chat, and the person had no way to get one of their own.
      // The fix is not "add a download button to every file" — that is how a chat log
      // turns into a pile of the model's leftovers. The engine now says which files are
      // attachments (the model wrote them into .cws/assistant_attachments/ on purpose),
      // and this check makes the page honour that split: exactly one control, on the one
      // file it belongs to, and the working material stays folded and un-downloadable.
      if (want('save-as', 'only the attachments folder gets a save it where I like control')) {
        transcript?.clear()
        transcript?.apply({ kind: 'turnStart', d: { turn_id: 1, model: 'selftest', session_id: 0 } })
        transcript?.apply({ kind: 'text', d: 'made you a report' })
        transcript?.addFiles([
          { path: '.cws/assistant_attachments/report.pdf', name: 'report.pdf', mime: 'application/pdf', bytes: 41000, kind: 'attachment' },
          { path: 'metr_report.html', name: 'metr_report.html', mime: 'text/html', bytes: 430006, kind: 'artifact' },
          { path: '.cws/probe.py', name: 'probe.py', mime: 'text/x-python', bytes: 412, kind: 'scratch' },
          { path: '.cws/page.html', name: 'page.html', mime: 'text/html', bytes: 9200, kind: 'scratch' },
          { path: 'src/a.ts', name: 'a.ts', mime: 'text/plain', bytes: 1200, kind: 'edit' },
          { path: 'src/b.ts', name: 'b.ts', mime: 'text/plain', bytes: 900, kind: 'edit' },
        ])
        transcript?.apply({ kind: 'turnEnd', d: { turn_id: 1, finish: 'stop' } })
        await new Promise((r) => setTimeout(r, 60))
        const saves = [...document.querySelectorAll('.attach-save')]
        if (saves.length !== 1)
          fail(
            `expected exactly one Save control for six reported files, got ${saves.length}: ` +
              saves.map((b) => b.getAttribute('aria-label')).join(', '),
          )
        else {
          const btn = saves[0] as HTMLButtonElement
          const unit = btn.closest('.attachunit')
          const chip = unit?.querySelector('.filechip') as HTMLElement | null
          if (!chip || chip.dataset.path !== '.cws/assistant_attachments/report.pdf')
            fail('the Save control is not attached to the attachment chip')
          // Working material must not be reachable by a Save click, and must not be a
          // chip in the open strip either: it is reference, folded in one line.
          const refs = [...document.querySelectorAll('.refgroup .filechip')]
          if (!refs.some((c) => (c as HTMLElement).dataset.path === '.cws/probe.py'))
            fail('the scratch copy of a fetched page vanished from the turn entirely')
          if (!(document.querySelector('.refgroup .reflist') as HTMLElement | null)?.hidden)
            fail('reference files are open by default')
          // The hook gets the attachment's own path, and the button answers.
          const realDeliver = transcript?.onDeliver ?? null
          let asked = ''
          if (transcript) transcript.onDeliver = async (p) => { asked = p; return 'saved to /tmp/report.pdf' }
          btn.click()
          await new Promise((r) => setTimeout(r, 80))
          if (asked !== '.cws/assistant_attachments/report.pdf') fail(`Save asked for "${asked}"`)
          // The control is an icon, so its answer is state, not a word: a tick in the glyph, the
          // `.done` class, and a title that says what happened. The reader was looking at a file
          // dialog while this happened, which is exactly why a word they may never see is not proof.
          if (!btn.classList.contains('done')) fail('Save did not answer: the glyph never showed the tick')
          if (!/^Saved /.test(btn.title)) fail(`Save left the button titled "${btn.title}"`)
          if (!btn.querySelector('svg.act-ico')) fail('the Save control lost its glyph')
          // A failure leaves the control usable rather than stuck on "Saving…", and the
          // reason reaches the toast rather than the console.
          if (transcript)
            transcript.onDeliver = async () => {
              throw new Error('the file went away')
            }
          const btn2 = document.querySelector('.attach-save') as HTMLButtonElement
          btn2.click()
          await new Promise((r) => setTimeout(r, 80))
          // A failure puts the plain download glyph back rather than leaving a tick that lied, and
          // leaves the button usable; the reason goes to the toast, not the console.
          if (btn2.classList.contains('done')) fail('a failed save left the button claiming success')
          if (btn2.disabled) fail('a failed save left the button disabled')
          if (transcript) transcript.onDeliver = realDeliver
        }
        transcript?.clear()
        pageLog('attachments ok (one Save control, on the attachment only; working files stay folded)')
      }

      // --- right-click gives app actions, not Back/Forward/Reload --------------
      if (want('context-menu', 'right-click gives app actions, not Back/Forward/Reload')) {
        transcript?.clear()
        transcript?.apply({ kind: 'turnStart', d: { turn_id: 1, model: 'selftest', session_id: 0 } })
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
      // neither of them true. It now buckets as no-workspace, the group says so in those
      // words, and registering the folder is refused by the engine and said out loud in
      // the UI.
      if (want('temp-folder', 'the temp folder is no workspace , not a folder')) {
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
          fail('a session in /tmp is not bucketed with the no-workspace conversations')
        if (wsGroups.some((g) => g.path === '/tmp'))
          fail('/tmp is drawn as a workspace group of its own')
        const labels = wsGroups.map((g) => g.label)
        if (bucket && bucket.label !== 'No workspace')
          fail(`the no-workspace group is labelled ${JSON.stringify(bucket.label)}, want "No workspace"`)
        // The chip is where the disagreement was. The sidebar filed such a conversation under
        // no-workspace and the engine tells the model it has none, and the chip printed `tmp`
        // - the name of a folder nobody chose - so the three said different things in front of
        // the person.
        if (wsChipText('/tmp') !== 'No workspace')
          fail(`the workspace chip calls the temp folder ${JSON.stringify(wsChipText('/tmp'))}`)
        if (wsChipText('/home/u/cwoffee-workspace') !== 'cwoffee-workspace')
          fail('a real folder lost its name in the chip')
        if (wsChipText('') !== 'no folder')
          fail(`a conversation with no path at all reads ${JSON.stringify(wsChipText(''))} in the chip`)
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
      if (want('session-folder', 'a session keeps the folder it started in')) {
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
          await frame(0)
          if (!document.querySelector('.empty-ws'))
            fail('the greeting does not say which folder the draft will use')
          const after = await invoke<Session[]>('list_sessions')
          if (after.length !== before.length)
            fail(`the + created ${after.length - before.length} session row(s); rows appear on first message`)
          draftWorkspace = ''
        }
        pageLog('workspace sidebar ok')

      // --- the folder a new session will work in, in one press -----------------
      // Reported as: "initial start screen of conversation, let's make the interface of the workspace
      // selector better: list added workspaces + two static buttons to add or clear". What it was:
      // the greeting named a folder only *after* one had been picked, and the only way to pick one
      // was a native file dialog - so "use the folder I already added" was a browse, and working
      // without a folder was something you could only end up in by not choosing.
      if (want('workspace-picker', 'the folder a new session will work in, in one press')) {
        if (await blind('the workspace picker')) {
          // counted skip; blind() logged why
        } else {
          const verdict: string[] = []
          if (!(await settleTurn('before the workspace picker check'))) fail('host would not go idle')
          // The app's own "new conversation": the row exists, but a row alone is not the greeting. The
          // host's switch is queued and can land after a busy turn unwinds, so the check waits for the
          // greeting itself rather than reading the page the instant after asking.
          newSession()
          await frame(0)
          let row = document.querySelector('button.empty-ws') as HTMLButtonElement | null
          for (let i = 0; i < 20 && !row; i++) {
            await new Promise((r) => setTimeout(r, 100))
            row = document.querySelector('button.empty-ws') as HTMLButtonElement | null
          }
          draftWorkspace = ''
          await frame(0)
          row = document.querySelector('button.empty-ws') as HTMLButtonElement | null
          if (!row)
            fail(`the greeting shows no workspace row to open a picker from (persona ${personaNow}, viewId ${viewId})`)
          else {
            if (!/no workspace selected/i.test(row.textContent ?? ''))
              verdict.push(`an unchosen folder reads "${row.textContent?.trim()}" instead of saying so`)
            // A folder in the list to find. Prefer one that is already registered; the scratch
            // folder derived from the app's own root is the fallback, and it is the same folder the
            // other workspace cases file their rows in.
            let dir = workspaces[0]?.path ?? ''
            if (!dir) {
              const home = hostState?.workspace ?? ''
              const guess = home ? home.replace(/\/+$/, '').split('/').slice(0, -1).join('/') : ''
              if (guess && guess !== '/' && !isTempRoot(guess)) {
                await invoke('add_workspace', { path: guess }).catch((e) => pageLog(`add_workspace for the picker: ${e}`))
                await loadWorkspaces()
                dir = workspaces[0]?.path ?? ''
              }
            }
            row.click()
            await frame(0)
            const panel = document.querySelector('.pop') as HTMLElement | null
            if (!panel) verdict.push('pressing the greeting\'s folder row opens nothing')
            else {
              const field = panel.querySelector('[data-ws-filter]') as HTMLInputElement | null
              // Focus lands on the effect that mounts the panel, which is a tick behind the click on
              // some cores and two on others; a single frame of patience made this check flaky rather
              // than wrong. The claim is unchanged - typing must go into the filter - only the wait
              // for the app to put it there is honest.
              if (field)
                for (let i = 0; i < 8 && document.activeElement !== field; i++) {
                  await new Promise((r) => setTimeout(r, 80))
                }
              const items = [...panel.querySelectorAll('[data-ws-pick-row]')] as HTMLElement[]
              const actNew = panel.querySelector('[data-ws-pick-new]')
              const actNone = panel.querySelector('[data-ws-pick-none]')
              const seam = panel.querySelector('.wsp-sep')
              if (!field) verdict.push('the picker has no filter field')
              else if (document.activeElement !== field && !document.hasFocus())
                // The element is focused by an effect that mounts the panel; when the window does not
                // hold the desktop's focus, `activeElement` can lag behind that effect by an arbitrary
                // amount because nothing is being painted. The claim is about where typing would go in
                // a page the user is looking at, so an unfocused window cannot answer it.
                pageLog('selftest note: the picker filter-focus claim was not graded; this window does not hold the desktop focus, and a page nothing paints applies mount effects late')
              else if (document.activeElement !== field)
                verdict.push('the picker opened without the filter focused, so typing goes nowhere')
              if (!actNew || !actNone) verdict.push('the picker is missing one of its two standing alternatives')
              if (!seam) verdict.push('the two alternatives are not separated from the folder list')
              else if (actNew && seam.compareDocumentPosition(actNew) & Node.DOCUMENT_POSITION_FOLLOWING) {
                // fine: the actions come after the seam
              } else if (actNew)
                verdict.push('"New workspace" sits above the seam, inside the folder list')
              if (field && items[0]) {
                const fr = field.getBoundingClientRect()
                const ir = items[0].getBoundingClientRect()
                if (ir.top < fr.top) verdict.push('the folder list starts above the filter field')
              }
              if (dir) {
                if (!items.length) verdict.push(`a workspace is registered (${dir}) but the picker lists none`)
                else if (!items.some((b) => (b.textContent ?? '').includes(dir)))
                  verdict.push(`the registered folder ${dir} is not offered by name or path in the picker`)
                else {
                  // The filter has to answer to typing, not only to existing.
                  if (field) {
                    field.value = 'qqzzx-not-a-folder'
                    field.dispatchEvent(new Event('input', { bubbles: true }))
                    await frame(0)
                    if (panel.querySelectorAll('[data-ws-pick-row]').length)
                      verdict.push('the filter left every folder in the list while the text could not match one')
                    if (!panel.querySelector('[data-ws-pick-empty]'))
                      verdict.push('a filter that matches nothing says nothing')
                    field.value = ''
                    field.dispatchEvent(new Event('input', { bubbles: true }))
                    await frame(0)
                    if (!panel.querySelectorAll('[data-ws-pick-row]').length)
                      verdict.push('clearing the filter did not bring the list back')
                  }
                  // Choosing one of them: the panel closes and the greeting says it.
                  const hit = [...panel.querySelectorAll('[data-ws-pick-row]')].find((b) =>
                    (b.textContent ?? '').includes(dir),
                  ) as HTMLButtonElement | undefined
                  hit?.click()
                  await frame(0)
                  if (draftWorkspace !== dir) verdict.push(`choosing ${dir} left the draft in "${draftWorkspace}"`)
                  if (document.querySelector('.pop')) verdict.push('the picker stayed open after a folder was chosen')
                  if (!(row.textContent ?? '').toLowerCase().includes(dir.split('/').pop() ?? ''))
                    verdict.push(`the greeting still reads "${row.textContent?.trim()}" after ${dir} was chosen`)
                }
              } else pageLog('selftest note: no workspace is registered, so the picker list half of this check had nothing to list')
              // "No workspace selected" has to be choosable, not just readable.
              row.click()
              await frame(0)
              const none = document.querySelector('.pop [data-ws-pick-none]') as HTMLButtonElement | null
              if (!none) verdict.push('the picker will not offer the way back to no folder')
              else {
                none.click()
                await frame(0)
                if (draftWorkspace) verdict.push(`"No workspace selected" left the draft in ${draftWorkspace}`)
                if (!/no workspace selected/i.test(row.textContent ?? ''))
                  verdict.push(`after choosing none the greeting reads "${row.textContent?.trim()}"`)
              }
              // A panel that runs off the window is a panel whose bottom row cannot be pressed.
              const pr = (document.querySelector('.pop') ?? panel)?.getBoundingClientRect()
              if (pr && (pr.bottom > window.innerHeight + 1 || pr.top < -1 || pr.right > window.innerWidth + 1 || pr.left < -1))
                verdict.push(
                  `the picker hangs outside the window (top ${Math.round(pr.top)}, bottom ${Math.round(pr.bottom)} of ${window.innerHeight})`,
                )
              if (panel.contains(row)) verdict.push('the picker contains the row that opened it, so the two overlap')
            }

            // --- the composer chip opens this picker, not the folder dialog -------
            //
            // The complaint this answers: "clicking the workspace under the composer doesn't open the
            // native picker immediately, instead it opens a popover" - which is what it now does, and
            // what it should do, because choosing among folders you already have is the common case and
            // a native dialog can only show the filesystem. The dialog is still reachable, under the
            // seam, labelled "New workspace".
            newSession()
            await frame(0)
            const chip = document.querySelector('button.wsrow') as HTMLButtonElement | null
            if (!chip) verdict.push('the composer has no workspace chip to open a picker from')
            else {
              chip.click()
              await frame(0)
              const pop2 = document.querySelector('.pop') as HTMLElement | null
              if (!pop2 || !pop2.querySelector('[data-ws-filter]'))
                verdict.push('the chip under the composer does not open the folder picker - it went to a dialog, or to nothing')
              else {
                const cr = chip.getBoundingClientRect()
                const qr = pop2.getBoundingClientRect()
                if (cr.width > 0 && qr.top > cr.bottom + 48 && qr.bottom < cr.top - 48)
                  verdict.push(
                    `the picker opened ${Math.round(Math.min(Math.abs(qr.top - cr.bottom), Math.abs(cr.top - qr.bottom)))}px away from the chip that asked for it`,
                  )
                const lst = pop2.querySelector('.wsp-list') as HTMLElement | null
                if (!lst) verdict.push('the picker has no folder list box')
                else {
                  const cs = getComputedStyle(lst)
                  const maxH = parseFloat(cs.maxHeight)
                  if (!Number.isFinite(maxH) || maxH <= 0 || cs.overflowY !== 'auto')
                    verdict.push(
                      `the folder list is not a bounded scroll box (max-height ${cs.maxHeight}, overflow-y ${cs.overflowY}); a long list has to scroll, not grow past the window`,
                    )
                }
                if (workspaces.length > 1 && !pop2.querySelector('[data-ws-pick-count]'))
                  verdict.push(`a bounded box holding ${workspaces.length} folders does not say how many are behind it`)
                closeMenu()
                await frame(0)
              }
            }

            // --- the exchange rule, stated inside the picker --------------------
            //
            // "An existing conversation where an exchange happened cannot change workspace (or clear)."
            // The chip already refuses; this checks the refusal is also visible where the ask actually
            // happens - the two standing actions must be closed, must say why, and must not move the
            // folder if something forces them.
            const spoken = sessions.find((s) => s.message_count > 0 && s.workspace && !isTempRoot(s.workspace))
            if (!spoken)
              pageLog('selftest note: no conversation with messages inside a real folder, so the exchange rule inside the picker was not exercised')
            else {
              openSession(spoken.id)
              for (let i = 0; i < 20 && viewId !== spoken.id; i++) await new Promise((r) => setTimeout(r, 100))
              await renderTick()
              if (!wsLocked)
                verdict.push(`conversation ${spoken.id} (${spoken.message_count} messages in ${spoken.workspace}) is not reported as folder-locked`)
              const chip2 = document.querySelector('button.wsrow') as HTMLButtonElement | null
              if (chip2 && !chip2.disabled)
                verdict.push('the chip is still pressable in a conversation that has already exchanged')
              openMenu('wspick', (chip2 ?? row ?? document.body) as HTMLElement)
              await frame(0)
              const pop3 = document.querySelector('.pop') as HTMLElement | null
              if (!pop3) verdict.push('the picker cannot be shown over a locked conversation, so its refusal is never seen')
              else {
                const act = [...pop3.querySelectorAll('[data-ws-pick-new], [data-ws-pick-none]')] as HTMLButtonElement[]
                if (act.length !== 2)
                  verdict.push(`the picker's standing actions are not both there (${act.length} of 2)`)
                else {
                  const pressable = act.filter((b) => !b.disabled)
                  if (pressable.length)
                    verdict.push(`${pressable.length} workspace action(s) can still be pressed in a conversation that has already exchanged`)
                  else if (!act.every((b) => /keeps its folder/i.test(b.getAttribute('title') ?? '')))
                    verdict.push('the closed actions do not say the rule that closed them')
                  const before = hostState?.workspace ?? ''
                  act[0].click()
                  act[1].click()
                  await frame(2)
                  const after = hostState?.workspace ?? ''
                  if (after !== before)
                    verdict.push(`a closed action still moved the conversation's folder (${before} -> ${after})`)
                }
                closeMenu()
                await frame(0)
              }
            }
            closeMenu()
            draftWorkspace = ''
            if (verdict.length) fail(`workspace picker: ${verdict.join('; ')}`)
            else
              pageLog(
                `workspace picker ok (the greeting says "no workspace selected" until one is, ${
                  dir ? `offers ${dir} by name and path, filters it away and back,` : 'holds its filter and its two alternatives,'
                } the chip under the composer opens this same picker rather than a dialog, the list is a bounded scroll box, and a conversation that has already exchanged has both actions closed with the rule on them)`,
              )
          }
        }
      }

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
      if (rafStalls > 0)
        pageLog(
          `selftest note: ${rafStalls} frame wait(s) settled on the timeout floor because requestAnimationFrame ` +
            'stopped firing; the window was not being composited for part of this run',
        )
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
          `${hits.length} hits, models=${ms.length} picked=${pick}` +
          (onlyTerms.length
            ? ` [TARGETED ${groups.ran} named group(s) ran, ${groups.skipped.length} not run: ${groups.skipped.join(', ')}]`
            : ''),
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
    if (e.key === 'Escape') {
      // Esc unwinds one layer at a time: the basket before the popup before the screen.
      if (picking) {
        clearPick()
        return
      }
      if (menu || settings) {
        closeMenu()
        closeSettings()
        return
      }
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
      // The pinned row at the foot of the sidebar. This used to be `footer` and reported the bottom
      // toolbar; the toolbar is gone (it cost a band across both panes), and the row that carries its
      // three surviving controls now lives in the sidebar - so the startup watchdog keeps the same
      // signal, "the shell's chrome rendered", without naming a bar that no longer exists.
      sidefoot: !!document.querySelector('.side-foot [data-side-id]'),
      stream_el: !!document.querySelector('.transcript'),
      empty_mode: !!document.querySelector('.empty'),
      body_bg: getComputedStyle(document.body).backgroundColor,
      overlay_present: !!document.querySelector('vite-error-overlay'),
      text_len: document.body.innerText.trim().length,
      device_pixel_ratio: window.devicePixelRatio,
      // The opening screen's picture, named and sized. Reported because a missing asset inside a
      // desktop bundle is silent - no console, no visible hole, just the wrong drawing - and this
      // line is the only place the installed build says what it actually loaded. The name also
      // shows which of the two files the motion setting picked.
      cup: (() => {
        const img = document.querySelector<HTMLImageElement>('.cupwrap .cup')
        if (!img) return 'none'
        // An armed picture's `src` is a blob URL, which would make this line name a random UUID.
        // The module knows which shipped asset it built the copy from, so the log still names the
        // file - and says that the pause in it was rewritten at runtime.
        const armed = armedFromUrl(img)
        const file = armed
          ? `${(armed.split('/').pop() ?? '?')}+random-hold`
          : (img.getAttribute('src') ?? '').split('/').pop() ?? '?'
        if (!img.complete) return `${file}:loading`
        return img.naturalWidth ? `${file}:${img.naturalWidth}x${img.naturalHeight}` : `${file}:did-not-decode`
      })(),
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
      transcript.onFollowState = (s) => {
        followState = s
      }
      // The loop note is one per turn, and it is a note rather than a block in the transcript: the
      // model did not say it, the app did. It says what was recognised, because "stuck" without the
      // repeated phrase is a guess the reader cannot check - and the preview that used to let them
      // check by reading is gone by design.
      transcript.onLoop = (f) => {
        // `say`, not `noteToast`: a migration note is something the app did, and this is bad news
        // about the model that the reader can act on. The dwell is long because the action - Stop -
        // is in the composer and has to be reached.
        say(`${describeLoop(f)} — that looks like a loop. Stop ends the turn.`, true, 11000)
        pageLog(`loop note: ${describeLoop(f)} after ${f.words} words / ${f.deltas} deltas`)
      }
      // The display settings are the app's, so they arrive from the store before the
      // first transcript is painted: a conversation that rendered one way and then
      // re-rendered when a pref landed is a visible jump over something you were reading.
      void loadPrefs()
      invoke<string[]>('migration_notes')
        .then((notes) => {
          for (const n of notes) transcript?.showNote(n)
        })
        .catch((e) => pageLog(`migration_notes failed: ${String(e)}`))
      transcript.onToast = (msg, bad) => say(msg, !!bad)
      // The transcript no longer shows app-level notes at all (see `Transcript.onNote`), so
      // the chrome has to pick them up or they are simply gone.
      transcript.onNote = (text) => noteToast(text)
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
      // An attachment: the one file the model wrote *in order to hand it over* — it
      // chose `.cws/assistant_attachments/`, and nothing else in the conversation is
      // offered this control. Desktop asks the host to open a save dialog; a browser
      // page has no claim on your filesystem, so the bytes cross the wire and the
      // browser downloads them, which is the closest honest equivalent.
      //
      // `session` is part of the request rather than inferred host-side: a conversation
      // can live in any folder, and the path has to resolve against *that* conversation
      // — not whichever one the host happens to be working in. The host then refuses
      // anything that is not under that conversation's attachments folder, because this
      // path arrived in a tool result the model wrote.
      transcript.onDeliver = async (path: string) => {
        // Captured, not read off the outer binding inside the closure: `transcript` is
        // nullable to Svelte and the closure outlives this block.
        const tr = transcript
        const session = tr?.sessionId ?? 0
        if (!session) throw new Error('this conversation is not saved yet')
        if (transportKind === 'gateway') {
          const f = await invoke<{ name: string; mime: string; bytes: number; b64: string }>(
            'attachment_bytes',
            { session, path },
          )
          const bin = atob(f.b64)
          const buf = new Uint8Array(bin.length)
          for (let i = 0; i < bin.length; i++) buf[i] = bin.charCodeAt(i)
          const blob = new Blob([buf], { type: f.mime || 'application/octet-stream' })
          const url = URL.createObjectURL(blob)
          const a = document.createElement('a')
          a.href = url
          a.download = f.name
          document.body.append(a)
          a.click()
          a.remove()
          setTimeout(() => URL.revokeObjectURL(url), 5000)
          return `downloaded ${f.name}`
        }
        const saved = await invoke<string | null>('save_attachment_as', { session, path })
        return saved ? `saved to ${saved}` : 'not saved — you closed the dialog'
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
      onReconnect(() => void retryAttach(true))
      // The server said "you missed events". Some of them matter - `turnEnd`
      // most of all - so re-read state instead of leaving a half-truth on screen.
      // Force included: a client that was told it missed events has lost the right to
      // trust its own memory of whether the turn it started is still up.
      onResync(() => void retryAttach(true))
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
      // Two keys, not one. The bundle was built by scripts/selftest.sh, and the
      // process that launched it also has to say the harness is expected.
      //
      // The release binary embeds whatever `ui/dist` holds at build time, so a
      // production start straight after a harness run ships this same bundle - and
      // a build-time flag alone then runs the checks against the user's real
      // endpoint, their real config and their real session store. That happened:
      // two sessions appeared in the store, `mode` and `reasoning` were rewritten
      // in `config.toml`, and turns went to the live model. So the gate is armed
      // at runtime, and a bundle that is not armed says so instead of going quiet.
      void invoke<boolean>('selftest_armed')
        .then((armed) => {
          if (armed) setTimeout(() => void selfTest(), 1200)
          else
            pageLog(
              'self-test bundle, harness NOT armed: set CWOFFEE_SELFTEST=1 to run the checks. ' +
                'Nothing else happens here - the checks drive the configured endpoint, the config ' +
                'file and the session store, which is not what a production start should touch.',
            )
        })
        .catch((e) =>
          pageLog(`self-test bundle could not ask the host whether it is armed (${e}); not running`),
        )
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
    // A phone tab that was backgrounded mid-turn comes back with a dead socket and a
    // `busy` that will never be corrected by an event that already happened. See
    // `wakeCheck`. Deliberately not on `focus`: a desktop window that merely lost focus
    // was never frozen and has nothing to repair, and re-reading on every alt-tab costs
    // a round trip for a case that cannot happen.
    document.addEventListener('visibilitychange', onVisibility)
    return () => {
      window.removeEventListener('keydown', onApprovalKey)
      window.removeEventListener('keydown', onGlobalKey)
      window.removeEventListener('pointerdown', onDocDown)
      window.removeEventListener('resize', reposition)
      document.removeEventListener('visibilitychange', onVisibility)
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

<div class="app-shell flex h-full flex-col bg-paper text-ink" class:modal-open={settings !== null} bind:this={shellEl}>
  <!-- ============================ titlebar ============================ -->
  <div
    class="titlebar flex h-11 shrink-0 items-center gap-2 border-b border-hairline bg-paper-2 px-3"
    data-tauri-drag-region
  >
    <!-- Sidebar toggle first. Below 768px the sidebar is a drawer, and without this
         button the session list and search are simply unreachable — so on a phone it is
         the one control that is always here, and it sits where the thumb already is.
         Above 768px the stylesheet hides it and the row opens with the mark. -->
    <button
      class="iconbtn sidemenu"
      onclick={() => setSidebar(!sidebarOpen)}
      aria-label="Sessions"
      aria-expanded={sidebarOpen}
    >
      <svg width="16" height="16" viewBox="0 0 14 14"><path d="M2 4h10M2 7h10M2 10h10" stroke="currentColor" stroke-width="1.3"/></svg>
    </button>
    <img src="/icon.png" alt="" class="h-5 w-5 rounded" />
    <span class="text-[13px] font-medium tracking-tight" data-tauri-drag-region>cwoffee</span>
    <!-- The conversation this window is looking at, when there is one.

         The model name used to sit here, and it read as a third thing in a 44px row
         that already had a mark and a word: eleven grey pixels of endpoint id that
         named nothing you could act on from the titlebar. It is on the composer's
         model chip instead — one tap from the control that changes it, and where the
         rest of the composer's state lives. With no conversation open the wordmark is
         the whole title, so nothing is added. -->
    {#if viewTitle}
      <span class="tb-sep" aria-hidden="true"></span>
      <span class="tb-title" title={viewTitle} data-tauri-drag-region>{viewTitle}</span>
    {/if}

    {#if transportKind === 'tauri'}
      <!-- Minimise / maximise / close belong to the frameless desktop window, and only
           there. In a browser tab they had nothing to act on: `win` is never resolved
           outside Tauri, so all three buttons were inert decorations whose handlers
           could only log a failure. They are drawn where they work. -->
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
    {/if}
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
        <div class="modegrid flex-1" class:chat={personaNow === 'chat'}>
          <button
            class="modesel"
            role="tab"
            aria-selected={personaNow === 'cowork'}
            title="the coworker: tools, a workspace, and the approval cards that go with them"
            onclick={() => void setPersona('cowork')}
          >Cowork</button>
          <button
            class="modesel"
            role="tab"
            aria-selected={personaNow === 'chat'}
            title="nothing but the model: no tools, no folder, one line of system prompt"
            onclick={() => void setPersona('chat')}
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

      <!-- The heading stays out of the scrolling list on purpose. It is not a label that happens to
           sit at the top of the thing it names: it carries the fold-all and add-folder controls, and a
           control that scrolls away with the conversations is a control you cannot reach when the list
           is what you are lost in. Same reason the sidebar footer is pinned. -->
      {#if !hits.length}
        <div class="sesshead">
          {#if personaNow === 'chat'}
            <span>Chats{chatSessions.length ? ` (${chatSessions.length})` : ''}</span>
          {:else}
            <span>Workspaces</span>
            <span class="sesshead-r">
              <button
                class="iconbtn ws-fold"
                aria-label="Fold every folder"
                title="Fold every folder"
                onclick={foldAll}
              >
                <svg width="12" height="12" viewBox="0 0 12 12"><path d="M3 7.4L6 4.4L9 7.4" stroke="currentColor" stroke-width="1.3" fill="none" stroke-linecap="round"/></svg>
              </button>
              <button
                class="iconbtn ws-fold"
                aria-label="Unfold every folder"
                title="Unfold every folder"
                onclick={unfoldAll}
              >
                <svg width="12" height="12" viewBox="0 0 12 12"><path d="M3 4.4L6 7.4L9 4.4" stroke="currentColor" stroke-width="1.3" fill="none" stroke-linecap="round"/></svg>
              </button>
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
            </span>
          {/if}
        </div>
      {/if}

      <!-- The list scrolls inside the sidebar. Without min-h-0 a flex item's
           min-height defaults to `auto`, i.e. its content height, so N sessions
           grew the sidebar, which grew the row, which pushed the composer and
           the telemetry bar off-screen. -->
      <div class="sesslist min-h-0 flex-1 overflow-y-auto">
        {#snippet sessRow(s: Session)}
                <div
                  class="sessrow {viewId === s.id ? 'active' : ''} {unread.has(s.id) ? 'unread' : ''} {picked.has('s' + s.id) ? 'picked' : ''}"
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
                      aria-pressed={picking ? picked.has('s' + s.id) : undefined}
                      onclick={(e) => rowClick(e, 's' + s.id, () => openSession(s.id))}
                    >
                      {#if picking}
                        <span class="pickbox" aria-hidden="true">
                          {#if picked.has('s' + s.id)}
                            <svg width="10" height="10" viewBox="0 0 10 10"><path d="M2.4 5.2L4.2 7L7.6 3.2" stroke="currentColor" stroke-width="1.6" fill="none" stroke-linecap="round"/></svg>
                          {/if}
                        </span>
                      {/if}
                      <span
                        class="sess-dot"
                        class:busy={runningSessions.has(s.id)}
                        aria-hidden="true"
                        title={runningSessions.has(s.id) ? 'answering' : ''}></span>
                      <span class="sess-title" data-sess={s.id} data-naming={namingShownFor(s) ? 'on' : undefined}>{namingOf(s)}{#if namingShownFor(s) && namingText}<span class="type-caret" aria-hidden="true"></span>{/if}</span>
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
        {/snippet}
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
        {#if personaNow === 'chat'}
        <!-- Chat owns no folder, so the folder tree is not a narrower view of the same
             thing here - it is the wrong thing. Chats get a flat list, newest first. The heading for
             it is pinned above the scroller, not in here. -->
        {#each chatSessions as s (s.id)}
          {@render sessRow(s)}
        {/each}
        {#if !chatSessions.length}
          <div class="pickhint">No chats yet. This one has no folder and no tools — write to it like a person, not a workspace.</div>
        {/if}
        {:else}
        <!-- The "Workspaces" heading and its fold / add controls are pinned above the scroller; the
             add-a-folder form that the `+` opens belongs under them, in here, because it is part of
             the list it adds to. -->
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
            <div class="wsrow {picked.has('w' + g.path) ? 'picked' : ''}">
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
                <button
                  class="wsbtn"
                  aria-expanded={open}
                  aria-pressed={picking ? picked.has('w' + g.path) : undefined}
                  onclick={(e) => rowClick(e, 'w' + g.path, () => toggleWs(g.path))}
                >
                  {#if picking}
                    <span class="pickbox" aria-hidden="true">
                      {#if picked.has('w' + g.path)}
                        <svg width="10" height="10" viewBox="0 0 10 10"><path d="M2.4 5.2L4.2 7L7.6 3.2" stroke="currentColor" stroke-width="1.6" fill="none" stroke-linecap="round"/></svg>
                      {/if}
                    </span>
                  {/if}
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
                {@render sessRow(s)}
              {/each}
            {/if}
          </div>
        {/each}
        {/if}
      {/if}
      </div>
      {#if picking && picked.size}
        <div class="bulkbar" role="toolbar" aria-label="Bulk delete">
          <span class="bulk-n">
            {pickedSessions.length} conversation{pickedSessions.length === 1 ? '' : 's'}
            {#if pickedFolders.length}
              · {pickedFolders.length} folder{pickedFolders.length === 1 ? '' : 's'}
            {/if}
            {#if pickedRunning.length}
              · {pickedRunning.length} answering, skipped
            {/if}
          </span>
          <button
            class="pill px-2 py-1 text-[12px] {bulkArm ? 'armed' : ''}"
            onclick={() => void deletePicked()}
          >
            {bulkArm ? 'Confirm' : 'Delete'}
          </button>
          <button class="pill px-2 py-1 text-[12px]" onclick={clearPick}>Done</button>
        </div>
      {:else if picking}
        <!-- The gesture is invisible until somebody discovers it, so once the mode is on
             it gets said out loud. -->
        <div class="pickhint">Ctrl-click or ⌘-click to pick, Shift-click for a run, Esc to stop.</div>
      {/if}
      <div class="side-foot">
        <!-- Pinned to the end of the sidebar, outside the scrolling list: a control that
             scrolls away with the conversations is not a control you can find when the
             list is what you are lost in. -->
        <button class="side-item" aria-expanded={settings != null} onclick={() => openSettings()}>
          <!-- A cog, not a sun. The old glyph was a circle with rays, which is the shape
               every UI in the world uses for brightness — so in dark theme this button
               looked like a light-mode toggle sitting in the sidebar footer. Stroke is
               currentColor so the cog follows the theme instead of assuming one. -->
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
            <path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z"/>
            <circle cx="12" cy="12" r="3"/>
          </svg>
          <span>Settings</span>
        </button>

        <!-- The row that replaced the old bottom toolbar: who this is, and the three things a person
             actually switches or checks during a day. Icons, not words - the sidebar is 248px and a
             word costs 60px of it. Each one is a real button with a name on it, and each glyph is
             centred by `.iconbtn > svg { margin: auto }`, which is the rule the hamburger check exists
             to defend.

             The mark is the *still* cup. The artwork that animates, animates at the foot of the
             transcript, where it means "working"; the same picture moving inside the chrome would be a
             second gesture saying the same thing in a place where nothing is happening. -->
        <div class="side-id" data-side-id>
          <img
            class="side-id-mark"
            src="/cwoffee_idle_still.webp"
            alt=""
            width="26"
            height="26"
          />
          <span class="side-id-name">Cwoffee</span>
          <span class="side-id-acts">
            <button
              class="iconbtn sfi"
              data-menu
              aria-haspopup="menu"
              aria-expanded={menu?.kind === 'metrics'}
              aria-label="Session telemetry"
              title={`Telemetry — ${cacheLabel} cache, ${liveEstimate ? '\u2248' : ''}${fmt.rate(curDecode)} tok/s${ctxLimit ? `, ${ctxLabel} ctx` : ''}`}
              onclick={(e) => openMenu('metrics', e.currentTarget as HTMLElement)}
            >
              <!-- A mileage dial: the footer used to spell cache-hit rate and decode speed out in words,
                   and a dial is the same fact at a glance. -->
              <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                <path d="M4.2 18.5a9.5 9.5 0 1 1 15.6 0" />
                <path d="M12 15.2l4.6-4.9" />
                <path d="M4.2 18.5h3M16.8 18.5h3" />
              </svg>
              {#if dropped > 0}<span class="sfi-dot hot" aria-hidden="true" title="{dropped} dropped event(s)"></span>{/if}
            </button>
            <button
              class="iconbtn sfi"
              data-menu
              aria-haspopup="menu"
              aria-expanded={menu?.kind === 'gateway'}
              aria-label="Remote access"
              title={`Gateway — ${gwText}`}
              onclick={(e) => openMenu('gateway', e.currentTarget as HTMLElement)}
            >
              <!-- A laptop: the gateway is "another device can reach this one", so the picture is the
                   machine being reached, not a tower or a cloud. -->
              <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                <rect x="3" y="4.5" width="18" height="12" rx="2" />
                <path d="M2 20h20" />
              </svg>
              {#if gwDot === 'live'}<span class="sfi-dot live" aria-hidden="true" title="{gwText}"></span>{/if}
            </button>
            <button
              class="iconbtn sfi"
              aria-label="Toggle light or dark theme"
              title="Toggle light / dark"
              onclick={() => applyTheme(theme === 'dark' ? 'light' : 'dark')}
            >
              {#if theme === 'dark'}
                <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                  <circle cx="12" cy="12" r="4.2" />
                  <path d="M12 2.6v2.2M12 19.2v2.2M2.6 12h2.2M19.2 12h2.2M5.3 5.3l1.6 1.6M17.1 17.1l1.6 1.6M18.7 5.3l-1.6 1.6M6.9 17.1l-1.6 1.6" />
                </svg>
              {:else}
                <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                  <path d="M20 14.2A8.4 8.4 0 1 1 9.8 4a7 7 0 0 0 10.2 10.2z" />
                </svg>
              {/if}
            </button>
          </span>
        </div>
      </div>
    </aside>

    <!-- main -->
    <main class="flex min-w-0 flex-1 flex-col min-h-0" class:empty-mode={showEmpty}>
      {#if connNote}
        <!-- Rendered, not logged: a phone cannot open a console, and "no model"
             was sending people to fix a config file on the wrong machine. -->
        <div class="connbar" role="alert">
          <span class="connbar-txt">{connNote}</span>
          <button class="btn-ghost text-[11px]" onclick={() => void retryAttach(true)}>retry</button>
        </div>
      {/if}
      {#if showEmpty}
        <!-- Sibling of the composer, not a wrapper around it: `main` centres
             them as one group in empty mode. Spacer divs would absorb the free
             space above the composer and leave it pinned to the bottom, which
             is what they did. -->
        <div class="empty">
          <!-- The cup, not a dot. It is the one thing in this app that is not a
               rectangle, so it carries the warmth, and it moves on its own: a 39-frame
               loop that takes 1.6s and then holds its last frame for 5.04s before
               starting again. The hold is in the file's frame table (the last frame's
               duration is 5042ms) because an <img> of an animated image cannot be held
               by CSS or JS; measured in this app's own WebKit, the pixels go quiet for
               5.01s between cycles.
               Nothing is drawn over it. There used to be an SVG wisp of steam here —
               one cartoonish half-S, anchored on the coffee surface with numbers measured
               off the artwork, rising and fading once per cup cycle — and it still read as
               a second animation that had nothing to do with the picture: two loops running
               beside each other, one of them invented. The artwork carries the gesture. -->
          <div class="cupwrap" aria-hidden="true">
            <img class="cup" src={idleArt} alt="" draggable="false" bind:this={cupEl} />
          </div>
          <h1 class="empty-hi">{greeting.hi}</h1>
          <p class="empty-sub">{greeting.sub}</p>
          {#if personaNow !== 'chat'}
            <!-- Saying which folder an unwritten session belongs to is the difference between "a
                 session in reports/" and a guess - and it has to be said before one is picked, not
                 only after. The row is the opener of the picker: the folders already added, a filter
                 for when there are more than fit, and the two standing alternatives (add one, choose
                 none). Clearing is no longer a hidden ✕ on a chip that only appears once something
                 was chosen. -->
            <button
              class="empty-ws"
              data-ws-pick
              aria-haspopup="dialog"
              aria-expanded={menu?.kind === 'wspick'}
              title={draftWorkspace || 'No workspace selected: this session has no folder, and no tools write anywhere'}
              onclick={(e) => openMenu('wspick', e.currentTarget as HTMLElement)}
            >
              <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true"><path d="M1.5 3.2h3l1 1.3h5v4.8h-9z" fill="none" stroke="currentColor" stroke-width="1.1"/></svg>
              <span>{draftWsLabel || 'No workspace selected'}</span>
              <span class="empty-ws-chev" aria-hidden="true">▾</span>
            </button>
          {/if}
          <div class="tips" role="group" aria-label="Start with">
            {#each TIPS as t}
              <button class="tip" onclick={() => seedTip(t.text)}>{t.label}</button>
            {/each}
          </div>
        </div>
      {/if}
      <div class="scrollerwrap">
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
      {#if followState.away > 120}
        <!-- The way back down. The transcript no longer drags the view after every block of
             working, which means a reader who went looking at something further up stays there -
             and needs one obvious thing to press to be current again. It is sticky to the bottom
             of the transcript rather than floating in the page, because "scroll me to the newest
             line" is a request about the transcript. When a question is waiting, the button says
             so: that is the one case where being behind the fold has a consequence. -->
        <button
          class="jump"
          class:ask={followState.ask}
          onclick={() => transcript?.jumpToBottom()}
          aria-label={followState.ask
            ? 'Jump to the newest line: cwoffee is waiting for an answer'
            : 'Jump to the newest line'}
          title={followState.ask
            ? 'cwoffee is asking you something below'
            : `${Math.round(followState.away)}px below the bottom of the view`}
        >
          <span class="jump-ico" aria-hidden="true">↓</span>
          <span class="jump-txt">{followState.ask ? 'cwoffee is asking' : 'newest'}</span>
        </button>
      {/if}
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
              <!-- No clipboard button. A screenshot reaches the composer the same way it does in
                   every other program - paste it - and the shell-side read of the selection happens
                   in the paste handler either way. The icon was a second way to do the one thing,
                   and it sat in the row of controls a reader scans for mode, model and effort. -->

              {#if personaNow !== 'chat'}
              <button class="cbtn chip" data-menu data-approvals onclick={(e) => openMenu('mode', e.currentTarget as HTMLElement)} aria-expanded={menu?.kind === 'mode'}>
                <svg width="13" height="13" viewBox="0 0 14 14"><circle cx="7" cy="7" r="5.2" fill="none" stroke="currentColor" stroke-width="1.2"/><path d="M5.4 7h3.2M7 5.4v3.2" stroke="currentColor" stroke-width="1.1"/></svg>
                <span>{MODE_LABEL[mode]}</span>
                <svg class="chev" width="10" height="10" viewBox="0 0 10 10"><path d="M2.5 4 5 6.5 7.5 4" stroke="currentColor" stroke-width="1.2" fill="none"/></svg>
              </button>
              {:else}
              <!-- Nothing to approve where no tool can run. The tab says which mode this
                   is; a control that could only ever be ignored would be decoration. -->
              <span class="cbtn chip" data-approvals="none" title="Chat mode has no tools, so nothing needs approving">Chat</span>
              {/if}

              <div class="ml-auto flex items-center gap-1">
                <button class="cbtn chip" data-menu onclick={(e) => openMenu('model', e.currentTarget as HTMLElement)} aria-expanded={menu?.kind === 'model'}>
                  <svg width="13" height="13" viewBox="0 0 14 14"><path d="M7.8 1.6 3.4 7.6h2.6l-.8 4.8 4.4-6H7z" fill="currentColor"/></svg>
                  <span class="chip-label">{shortModel(model)}</span>
            {#if modelErr}<span class="chip-warn" title="Endpoint unreachable">!</span>{/if}
                  <svg class="chev" width="10" height="10" viewBox="0 0 10 10"><path d="M2.5 4 5 6.5 7.5 4" stroke="currentColor" stroke-width="1.2" fill="none"/></svg>
                </button>
                <button class="cbtn chip" data-menu onclick={(e) => openMenu('think', e.currentTarget as HTMLElement)} aria-expanded={menu?.kind === 'think'}>
                  <span>{THINK_LABEL[thinking]}{hostState?.reasoning_overridden ? ' · this chat' : ''}</span>
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

          {#if personaNow === 'chat'}
            <!-- Not hidden, because the space where the folder used to be would otherwise
                 read as a bug. It says what is true instead. -->
            <div class="wsrow ws-none">
              <svg width="13" height="13" viewBox="0 0 14 14"><path d="M1.8 4.2h3.4l1 1.4h6v5.6a.8.8 0 0 1-.8.8H2.6a.8.8 0 0 1-.8-.8z" fill="none" stroke="currentColor" stroke-width="1.1"/></svg>
              <span>Chat has no folder, and no tools to use one with</span>
            </div>
          {:else if wsEditing && !wsLocked}
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
                ? `This session works in ${wsShownText}. A session keeps its folder: start a new session to work somewhere else.`
                : isTauri()
                  ? 'Choose which folder the tools work in - the folders you have added, or a new one'
                  : `Host workspace: ${wsShownText} - click to pick from the folders the host knows`}
              onclick={(e) => openWorkspacePicker(e.currentTarget as HTMLElement)}
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

  <!-- ================================================================
       Settings bodies, written once and rendered twice.

       Every block below is drawn both in the popup that answers a passing
       question (the telemetry bar, the composer controls) and in the settings
       screen under its own heading. That sharing is the point: a control that
       exists in two copies is two chances to mean different things, and the
       app has already been bitten by exactly that once - the ceiling was set
       from a panel in one place and shown from a different number in another.
       ================================================================ -->
  {#snippet approvalsBody()}
    <Choices
      aria="Approval mode"
      value={mode}
      options={MODE_CHOICES}
      onpick={(v) => void pickMode(v as 'ask' | 'session' | 'auto')}
    />
    <p class="set-note">
      A yes covers this conversation only, and never a folder it does not already
      cover: a path outside the workspace is refused in every mode, including
      <span class="mono">run without asking</span>. The sandbox is the app's, not
      the model's, so no answer here widens what a command can reach.
    </p>
  {/snippet}

  {#snippet reasoningBody()}
    <Segmented
      aria="Thinking effort"
      value={thinking}
      options={THINK_CHOICES}
      onpick={(v) => void pickThinking(v as ReasoningLevel)}
    />
    <p class="set-note">{thinkingNote}</p>
  {/snippet}

  {#snippet reasoningDefaultBody()}
    <Segmented
      aria="Default thinking effort"
      value={thinkingDefault}
      options={THINK_CHOICES}
      onpick={(v) => void setThinking(v as ReasoningLevel, true)}
    />
    <p class="set-note">{thinkingDefaultNote}</p>
  {/snippet}

  {#snippet ceilingRow()}
    <div class="srow" data-set-ceiling>
      <div>
        <div class="srow-t">
          Context ceiling
          <b class="mono nowrap">{fmtTokens(hostState?.context_budget ?? 0)} tokens</b>
        </div>
        <div class="srow-h" data-budget-provenance>
          {#if hostState?.budget_pin}
            pinned by you at {fmtTokens(hostState.budget_pin)}: it survives a restart, and every
            client of this app reads the same number
          {:else if ctxLimit}
            derived: half of the {fmtTokens(ctxLimit)} token window this endpoint states -
            the other half is left for the answer
          {:else}
            derived: this endpoint has stated no window yet, so the app keeps a standing
            default until one is known
          {/if}
        </div>
      </div>
      <div class="srow-c">
        <input
          class="sinp num narrow mono"
          type="number"
          min="4096"
          step="1024"
          data-ceiling-input
          bind:value={budgetDraft}
          placeholder={String(hostState?.context_budget ?? 0)}
          aria-label="Context ceiling in tokens"
          title="Tokens. Below 4096 the app refuses: a prompt that small cannot hold the system prompt, the tool definitions and a tool result, and still leave room for an answer. Above the window the endpoint states it also refuses, because the server would cut the prompt before the meter ever filled."
        />
        <button
          class="set-btn primary"
          disabled={budgetBusy}
          title="Write this ceiling to the config file, then apply what the file says."
          onclick={pinBudget}
        >
          Pin
        </button>
        <button
          class="set-btn plain"
          disabled={budgetBusy || !hostState?.budget_pin}
          title="Let go of the pin. The ceiling goes back to half of whatever window this model's endpoint states, so it is stored by removing the line rather than by writing a number for it."
          onclick={() => void setBudget(null)}
        >
          Derive
        </button>
        <button
          class="set-btn"
          disabled={compacting || (hostState?.busy ?? false)}
          title={hostState?.busy
            ? 'A turn is running; compaction happens between turns'
            : 'Ask the model to summarize the older turns. Recent turns stay word for word; the note reports the token counts the core measured, not an estimate from this window.'}
          onclick={() => void compactNow()}
        >
          {compacting ? 'Compacting…' : 'Compact now'}
        </button>
      </div>
    </div>
    {#if budgetMsg}
      <div class="set-note" data-budget-note>{budgetMsg}</div>
    {/if}
  {/snippet}

  {#snippet sessionReadouts(showCeiling: boolean)}
    <div class="mbar-row">
      <span class="mlbl">context</span>
      <span
        class="mbar"
        title="prompt tokens against the ceiling the endpoint states; the tick marks the engine's own prompt budget, the line history is trimmed past"
      >
        <i style="width:{Math.min(100, ctxPct)}%"></i>
        {#if ctxLimit && hostState?.context_budget}
          <u
            style="left:{Math.min(100, (hostState.context_budget / ctxLimit) * 100)}%"
            title="{hostState.context_budget.toLocaleString()} tokens: the engine's prompt budget"
          ></u>
        {/if}
      </span>
      <span
        class="mval mono"
        title="{(hostState?.context_tokens ?? 0).toLocaleString()} tokens used against a {ctxLimit ? ctxLimit.toLocaleString() : 'unknown'} token window the endpoint itself stated. The percentage is the number worth watching; the exact counts live here rather than on screen."
      >
        {#if ctxLimit}
          {ctxPct}%
        {:else}
          no window reported
        {/if}
        {#if hostState?.context_budget}
          <small class="ctxtrim" title="history is trimmed past this, not past the model's window"
            >trims at {fmtTokens(hostState.context_budget)}</small
          >
        {/if}
      </span>
    </div>
    {#if showCeiling}
      {@render ceilingRow()}
    {/if}
    <div class="mbar-row">
      <span class="mlbl">prefix cache</span>
      <span class="mbar" title="how much of the prompt the server already had">
        <i class="ok" style="width:{Math.min(100, cacheHit)}%"></i>
      </span>
      <span class="mval mono">{cacheLabel}</span>
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
        <b>{turnMs ? (turnMs / 1000).toFixed(1) : '\u2014'}<small>s</small></b><span>this turn</span>
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
    <p class="set-note">
      Rates marked ≈ are estimated from characters: the server reports token counts only when a
      turn ends. Context is measured against the window the endpoint states, which is a
      measurement, not a reservation.
    </p>
  {/snippet}

  {#snippet devicesBody()}
    {#if transportKind !== 'tauri'}
      <div class="smsg">
        This window is served by the gateway running on the host machine, so it is already
        attached. Turn remote access on or off from the desktop app on that machine.
      </div>
      <p class="set-note">
        Plain http, no tls: anyone who can reach that address and read the link can drive this
        agent, workspace and tools included.
      </p>
    {:else}
      <div class="srow">
        <div>
          <div class="srow-t">Remote access</div>
          <div class="srow-h">
            {#if gwBusy}
              changing…
            {:else if gw?.running}
              running on <span class="mono">{gw.host}:{gw.port}</span>
            {:else if gwErr}
              not running: {gwErr}
            {:else if gw?.note && gw.note !== 'off'}
              not running: {gw.note}
            {:else}
              off: no other device can reach this app
            {/if}
          </div>
        </div>
        <div class="srow-c">
          <Switch
            on={!!gw?.running}
            disabled={gwBusy}
            aria="Turn remote access on or off"
            onpick={(next) => void setGateway(next)}
          />
        </div>
      </div>
      <div class="srow">
        <div>
          <div class="srow-t">Which machines may connect</div>
          <div class="srow-h">
            {#if gw?.running}
              bound now to <span class="mono">{gw.host}</span>
            {:else}
              used at the next start:
              {gwScope === 'lan' ? 'every interface (0.0.0.0)' : 'this machine only (127.0.0.1)'}
            {/if}
          </div>
        </div>
        <div class="srow-c">
          <Segmented
            aria="Which machines may connect"
            value={gwScopeShown}
            options={SCOPE_CHOICES}
            onpick={(v) => void setScope(v as 'loopback' | 'lan')}
          />
        </div>
      </div>
      {@const keep = uiPrefs.find((p) => p.key === 'gateway_on_exit')}
      {#if keep}
        <div class="srow" data-gw-keep>
          <div>
            <div class="srow-t" data-gw-keep-label>{keep.label}</div>
            <div class="srow-h">{keep.hint}</div>
            {#if gwPlan?.binary}
              <div class="srow-h mono" data-gw-keep-plan>
                a different process: {gwPlan.binary} on :{gwPlan.port} ({gwPlan.via}) · stop it
                later with {gwPlan.stop} · its own lines go to {gwPlan.log}
              </div>
            {:else}
              <div class="srow-h" data-gw-keep-plan="missing">
                this cannot be honoured from this app: there is no cwoffee-gateway next to it, in
                /usr/local/bin or on PATH. The gateway in this window is a task inside the app, so
                keeping it means starting the standalone one —
                <span class="mono">cargo build --release -p cwoffee-gateway</span>, or
                <span class="mono">scripts/install-desktop.sh</span>, which installs the copy the
                launcher uses.
              </div>
            {/if}
            {#if keep.value === 'on' && gwPlan && !gwPlan.running}
              <div class="srow-h" data-gw-keep-warn>
                on, but remote access is off right now, so this window has no gateway to hand over
              </div>
            {/if}
          </div>
          <div class="srow-c">
            <Switch
              on={keep.value === 'on'}
              disabled={prefBusy}
              aria={keep.label}
              onpick={(next) => void setPref('gateway_on_exit', next ? 'on' : 'off')}
            />
          </div>
        </div>
      {/if}
      {#if gw?.running}
        <div class="srow">
          <div>
            <div class="srow-t">Attached now</div>
            <div class="srow-h mono">{attachedLine(gw)} · token from {gw.token_source ?? 'none'}</div>
          </div>
        </div>
        <div class="slist">
          {#each gw?.urls ?? [] as u (u)}
            <div class="srow" data-gw-url={u}>
              <div class="mono" style="font-size:11.5px; word-break:break-all">{u}</div>
              <div class="srow-c">
                <button class="set-btn plain" onclick={() => void copyUrl(u)}>
                  {copiedUrl && copiedUrl === 'copied' ? 'Copied' : 'Copy link'}
                </button>
              </div>
            </div>
          {/each}
        </div>
      {/if}
      {#if gwErr}
        <div class="smsg bad" data-gw-error>{gwErr}</div>
      {/if}
      <p class="set-note">
        The link is the credential. A drive token can send turns, move the workspace and run
        sandboxed commands, so anyone who reads it can drive this agent. Nothing here is a
        second factor: keep the scope at this machine unless a device has to reach it.
      </p>
    {/if}
  {/snippet}

  {#snippet webBody()}
    {#if !webSearch}
      <div class="smsg wait" data-web-state="loading">
        {webSearchErr ? `Cannot read the search settings: ${webSearchErr}` : 'Reading the search settings…'}
      </div>
    {:else}
      <div class="srow" data-web-current>
        <div>
          <div class="srow-t">In use now</div>
          <div class="srow-h">{webSearch.describing}</div>
        </div>
      </div>
      <Choices
        aria="Search provider"
        value={webSearchPending ?? webSearch.backend}
        options={SEARCH_CHOICES}
        disabled={webSearchBusy}
        onpick={(v) => void chooseWebSearch(v)}
      />
      {#if (webSearchPending ?? webSearch.backend) === 'searxng'}
        <Field
          label="Instance URL"
          forId="cw-searxng-url"
          hint="A SearXNG instance with format=json enabled in its settings.yml, e.g. http://127.0.0.1:8080/search"
        >
          <input
            class="sinp mono"
            id="cw-searxng-url"
            type="text"
            placeholder="http://127.0.0.1:8080/search"
            bind:value={webSearchUrl}
            onchange={() => void chooseWebSearch('searxng')}
          />
        </Field>
      {/if}
      {#if (webSearchPending ?? webSearch.backend) === 'auto'}
        <div class="srow" data-web-auto>
          <div>
            <div class="srow-t">The instance Cwoffee runs itself</div>
            <div class="srow-h">{webSearch.auto?.note ?? 'checking…'}</div>
            <div class="srow-h mono">
              port {webSearch.auto_port} · idle {webSearch.auto_idle_secs}s · config
              {webSearch.auto_dir}
            </div>
          </div>
        </div>
        {#if !webSearch.runtime}
          <div class="smsg">
            No podman or docker on this machine, so Cwoffee cannot run an instance of its own.
            Search keeps working through DuckDuckGo and says so in every result until a runtime
            appears.
          </div>
        {/if}
      {/if}
      <div class="srow" data-web-probe>
        <div>
          <div class="srow-t">Try one search</div>
          <div class="srow-h">
            {webSearchProbe
              ? `${webSearchProbe.results.length} result${webSearchProbe.results.length === 1 ? '' : 's'} in ${webSearchProbe.ms}ms`
              : 'asks the provider directly, not the model, so a working provider and a working model stay separate facts'}
          </div>
        </div>
        <div class="srow-c">
          <button class="set-btn" disabled={webSearchBusy} onclick={() => void runWebSearchProbe()}>
            {webSearchBusy ? 'Searching…' : 'Run probe'}
          </button>
        </div>
      </div>
      {#if webSearchProbe && !webSearchProbe.error}
        <div class="slist" data-web-results>
          {#each webSearchProbe.results.slice(0, 3) as r (r.url)}
            <div class="srow">
              <div>
                <div class="srow-t">{r.title}</div>
                <div class="srow-h mono">{r.url}</div>
              </div>
            </div>
          {/each}
        </div>
        {#if webSearchProbe.note}
          <p class="set-note">{webSearchProbe.note}</p>
        {/if}
      {/if}
      {#if webSearchProbe?.error}
        <div class="smsg bad" data-web-error>{webSearchProbe.error}</div>
      {/if}
      {#if webSearchErr}
        <div class="smsg bad" data-web-error>{webSearchErr}</div>
      {/if}
      {#if webSearch.env_override}
        <p class="set-note">{webSearch.env_override}</p>
      {/if}
      {#if webSearch.saved_to}
        <p class="set-note">Saved to {webSearch.saved_to}.</p>
      {/if}
    {/if}
  {/snippet}

  {#snippet keysBody()}
    {#each SHORTCUTS as sc (sc.keys)}
      <div class="srow">
        <div class="srow-t">{sc.label}</div>
        <div class="srow-c mono nowrap">{sc.keys}</div>
      </div>
    {/each}
  {/snippet}
  {#snippet body(menu: Menu)}
      {#if menu.kind === 'mode'}
        <div class="pop-title">Approvals</div>
        {@render approvalsBody()}
      {:else if menu.kind === 'think'}
        <div class="pop-title">Thinking effort</div>
        {@render reasoningBody()}
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
      {:else if menu.kind === 'wspick'}
        <div class="wsp-search">
          <svg class="wsp-ico" width="13" height="13" viewBox="0 0 12 12" aria-hidden="true"><circle cx="5.4" cy="5.4" r="3.4" fill="none" stroke="currentColor" stroke-width="1.2"/><path d="m8 8 2.4 2.4" stroke="currentColor" stroke-width="1.2" stroke-linecap="round"/></svg>
          <input
            class="wsp-input"
            data-ws-filter
            bind:value={wsFilter}
            use:focusNow
            spellcheck="false"
            aria-label="Search workspace names or paths"
            placeholder="Search names or paths"
          />
        </div>
        {#if wsFiltered.length}
          <div class="wsp-list" data-ws-pick-list>
            {#each wsFiltered as w (w.path)}              <button
                class="pop-item wsp-item {draftWorkspace === w.path ? 'on' : ''}"
                role="menuitem"
                data-ws-pick-row
                onclick={() => pickDraftWorkspace(w.path)}
              >
                <svg class="wsp-ico" width="13" height="13" viewBox="0 0 12 12" aria-hidden="true"><path d="M1.5 3.2h3l1 1.3h5v4.8h-9z" fill="none" stroke="currentColor" stroke-width="1.1"/></svg>
                <span class="pop-lbl">{wsLabelOf(w)}</span>
                <span class="pop-sub mono">{w.path}</span>
              </button>
            {/each}
          </div>
        {:else}
          <div class="pop-empty" data-ws-pick-empty>
            {workspaces.length
              ? 'nothing matches that search'
              : 'no folders added yet - add one, or work with no folder'}
          </div>
        {/if}
        <!-- The list is a fixed-height box with its own scroll, not a cap. Hiding folders behind "the
             ten most recent" would make the ones you added a month ago undiscoverable, which is how a
             picker starts a duplicate folder. The count line is here so a short box does not read as a
             short list. -->
        {#if workspaces.length > 1}
          <div class="wsp-count" data-ws-pick-count>
            {workspaces.length} folder{workspaces.length === 1 ? '' : 's'}, most recent first
          </div>
        {/if}
        <!-- The two standing alternatives, under a seam rather than in the list: they are not
             folders, and putting them among the folders would make "New workspace" look like a
             folder someone had already named. -->
        <div class="wsp-sep" role="presentation"></div>
        <button
          class="pop-item wsp-act"
          role="menuitem"
          data-ws-pick-new
          disabled={wsPickLocked}
          title={wsPickLocked ? wsPickLockNote : ''}
          onclick={() => void newWorkspaceFromPicker()}
        >
          <svg class="wsp-ico" width="13" height="13" viewBox="0 0 14 14" aria-hidden="true"><path d="M7 2.6v8.8M2.6 7h8.8" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/></svg>
          <span class="pop-lbl">New workspace</span>
          <span class="pop-sub">{transportKind === 'tauri' ? 'Pick a folder; it joins the list and this session works in it' : 'Type a path on the host; it joins the list and this session works in it'}</span>
        </button>
        <button
          class="pop-item wsp-act"
          role="menuitem"
          data-ws-pick-none
          disabled={wsPickLocked}
          title={wsPickLocked ? wsPickLockNote : ''}
          onclick={clearDraftWorkspace}
        >
          <svg class="wsp-ico" width="13" height="13" viewBox="0 0 12 12" aria-hidden="true"><path d="M1.5 3.2h3l1 1.3h5v4.8h-9z" fill="none" stroke="currentColor" stroke-width="1.1"/><path d="M2 2.2 10.2 9.6" stroke="currentColor" stroke-width="1.1" stroke-linecap="round"/></svg>
          <span class="pop-lbl">No workspace selected</span>
          <span class="pop-sub">A session with no folder: nothing is read or written anywhere</span>
        </button>
      {:else if menu.kind === 'ws'}
        {@const wp = menu.wsPath ?? ''}
        {@const wrow = workspaces.find((w) => w.path === wp)}
        {@const wlabel = wrow ? wsLabelOf(wrow) : wp && !isTempRoot(wp) ? basenameOf(wp) : 'No workspace'}
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
            {@const inside = wsInside[wp] ?? -1}
            <button
              class="pop-item danger"
              role="menuitem"
              onclick={() => void deleteFolderAndRows(wp)}
            >
              <span class="pop-lbl">
                {wsDeleteArm === wp
                  ? 'Delete the folder again — it is armed'
                  : inside > 0
                    ? `Delete folder and its ${inside} conversation${inside === 1 ? '' : 's'}`
                    : 'Delete folder'}
              </span>
              <span class="pop-sub">
                {inside > 0
                  ? 'Every conversation filed here goes with it, chats and empty drafts included. Anything mid-turn is left alone and the folder stays.'
                  : 'Nothing is filed here, so this only drops the row'}
              </span>
            </button>
            <button
              class="pop-item"
              role="menuitem"
              onclick={() => void removeFolder(wp)}
            >
              <span class="pop-lbl">Take it off the sidebar</span>
              <span class="pop-sub">
                {inside > 0
                  ? 'Forgets the row only. The conversations stay, and the folder comes back because they are still in it.'
                  : 'The folder has no conversations in it'}
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
        {@render sessionReadouts(true)}
      {:else if menu.kind === 'gateway'}
        <div class="pop-title">Remote access</div>
        {@render devicesBody()}
      {:else if menu.kind === 'search'}
        <div class="pop-title">Web search</div>
        {@render webBody()}
      {:else if menu.kind === 'keys'}
        <div class="pop-title">Keyboard</div>
        {@render keysBody()}
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
  {/snippet}

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
      {@render body(menu)}
    </div>
  {/if}

  {#if settings}
    <div class="mscrim" onclick={closeSettings} role="presentation"></div>
    <div
      class="settings"
      role="dialog"
      aria-modal="true"
      aria-label="Settings"
      tabindex="-1"
      bind:this={settingsEl}
      style={settingsStyle()}
      use:rememberSize
    >
      <header class="set-head">
        <span class="set-title">Settings</span>
        <button class="set-x" onclick={closeSettings} aria-label="Close settings" title="Close (Esc)">Esc</button>
      </header>
      <div class="set-grid">
        <nav class="set-rail" aria-label="Settings categories">
          <div class="set-rail-lbl">Settings</div>
          {#each SET_PAGES as p (p.kind)}
            <button
              class="set-nav {settings.page === p.kind ? 'on' : ''}"
              data-set-tab={p.kind}
              onclick={() => gotoSettings(p.kind)}
              aria-current={settings.page === p.kind ? 'true' : undefined}
            >
              <svg
                class="set-nav-ic"
                width="15"
                height="15"
                viewBox="0 0 16 16"
                fill="none"
                stroke="currentColor"
                stroke-width="1.3"
                stroke-linecap="round"
                stroke-linejoin="round"
                aria-hidden="true"
              >
                <path d={p.icon}></path>
              </svg>
              <span class="set-lbl">{p.label}</span>
            </button>
          {/each}
        </nav>

        <div class="set-body" data-set-page={settings.page} bind:this={setBodyEl}>
          <h2 class="spage-title">{pageTitle}</h2>
          <p class="spage-desc">{pageDesc}</p>

          {#if settings.page === 'models'}
            <Section
              id="answering"
              title="The model answering now"
              desc="One model answers at a time in this conversation. Choosing another changes where the answer comes from; the conversation keeps its folder, its history and its approvals."
            >
              <div class="srow" data-current-model>
                <div>
                  <div class="srow-t mono">{hostState?.model || model || 'nothing picked yet'}</div>
                  <div class="srow-h" data-current-endpoint>{currentModelLine}</div>
                  <div class="capset" style="margin-top:6px">
                    {#each capLabel(currentCapRow ?? { id: model }) as c (c)}
                      <span class="cap">{c}</span>
                    {/each}
                  </div>
                </div>
                <div class="srow-c" style="min-width:250px">
                  <Select
                    aria="Choose the model that answers"
                    value={model}
                    options={MODEL_CHOICES}
                    mono
                    disabled={busy || !models.length}
                    onpick={(v) => void pickModel(v)}
                  />
                </div>
              </div>
              {#if modelErr}
                <div class="smsg bad" data-model-error>{modelErr}</div>
              {:else if !models.length}
                <!-- Three different reasons look the same from here unless they are
                     named: no host, no endpoint, and an endpoint that lists nothing. -->
                <div class="sempty" data-model-empty>
                  {#if !ipc.ok}
                    Not connected to the host, so no model list could be asked for: {ipc.msg}
                  {:else}
                    This endpoint lists no models. {model ? `Answering from ${model}.` : ''} Add the
                    endpoint below to store what it states, or start the server it points at.
                  {/if}
                </div>
              {:else}
                <p class="set-note">
                  The list is what this endpoint published. Capabilities marked “detected” were
                  inferred from the chat template the server serves rather than stated by it.
                </p>
              {/if}
            </Section>

            <Section
              id="endpoints"
              title="Add an endpoint"
              desc="Register another inference server without replacing the one in use. Nothing is written until Add endpoint; Add needs a completed discovery, so a guess cannot become configuration."
            >
              {#if pvMsg}
                <div class="smsg {pvBad ? 'bad' : 'ok'}" data-pv-msg>{pvMsg}</div>
              {/if}
              <div class="sfld-grid">
                <Field label="Endpoint name" forId="cw-pv-name" hint="A label for this list. It is not part of the address.">
                  <input class="sinp" id="cw-pv-name" type="text" bind:value={pvName} placeholder="the box on the LAN" />
                </Field>
                <Field
                  label="API compatibility"
                  hint="Which dialect the server speaks. Left on the default when the URL already says which it is."
                >
                  <Select
                    id="cw-pv-kind"
                    aria="API compatibility"
                    value={pvKind}
                    options={PROVIDER_KINDS}
                    onpick={(v) => (pvKind = v)}
                  />
                </Field>
                <Field
                  label="Base URL"
                  forId="cw-pv-url"
                  wide
                  hint={urlHintLine}
                >
                  <input
                    class="sinp mono"
                    id="cw-pv-url"
                    type="text"
                    spellcheck="false"
                    bind:value={pvUrl}
                    placeholder="http://192.168.0.122:8080/v1"
                  />
                </Field>
                <Field
                  label="API key"
                  forId="cw-pv-key"
                  hint="Only if the endpoint asks for one. A key typed here is written to the config file and never shown back on this screen."
                >
                  <input class="sinp mono" id="cw-pv-key" type="password" bind:value={pvKey} placeholder="not needed for a local server" />
                </Field>
              </div>
              <div class="pvacts">
                <button
                  class="set-btn primary"
                  disabled={pvBusy || probed.length === 0}
                  title="Write this endpoint and everything discovery found into the config file."
                  onclick={() => void addProvider()}
                >
                  Add endpoint{probed.length ? ` and ${probed.length} model${probed.length === 1 ? '' : 's'}` : ''}
                </button>
                <button class="set-btn" disabled={probing || !pvUrl.trim()} onclick={() => void probeProvider()}>
                  {probing ? 'Discovering…' : 'Discover models'}
                </button>
                <button class="set-btn" disabled={pvBusy || !pvUrl.trim()} onclick={() => void testProvider()}>
                  {pvBusy ? 'Contacting…' : 'Test connection'}
                </button>
                <button
                  class="set-btn plain"
                  disabled={!probed.length && !pvMsg}
                  onclick={() => {
                    probed = []
                    pvMsg = ''
                  }}
                >
                  Clear
                </button>
              </div>
              {#if probing}
                <p class="set-note">
                  Discovery asks the endpoint what each of its models can do: the window it states,
                  whether it accepts tool calls, whether it takes images, which thinking levels it
                  names back. It is one request per model, so on a machine that is also running one
                  of these models it can take a minute.
                </p>
              {:else if !pvUrl.trim() && provRows.length}
                <p class="set-note">
                  The fields start as whatever the endpoint in use has. Changing the URL describes a
                  new endpoint rather than editing that one.
                </p>
              {/if}
              {#if probed.length}
                <div class="slist" data-probed>
                  {#each probed as m (m.id)}
                    <div class="srow">
                      <div>
                        <div class="srow-t mono">{m.id}</div>
                        <div class="capset" style="margin-top:5px">
                          {#each capLabel(m) as c (c)}<span class="cap">{c}</span>{/each}
                        </div>
                      </div>
                    </div>
                  {/each}
                </div>
                <p class="set-note">
                  Each row above becomes a model you can open and edit. The window an endpoint states
                  is what the context meter, the derived ceiling and the core's own fold are built
                  from, which is why adding an endpoint through discovery keeps those numbers
                  measured instead of assumed.
                </p>
              {/if}
            </Section>

            <Section
              id="endpoints-list"
              title="Configured endpoints"
              desc="Endpoints this app has been told about, and what it believes about each model on them. An empty field means “ask the endpoint”: saving it empty removes the stored value rather than writing a placeholder for it."
            >
              {#snippet actions()}
                {#if provRows.length}
                  <button
                    class="set-btn plain"
                    onclick={() => {
                      openProv =
                        openProv.size === provRows.length ? new Set() : new Set(provRows.map((x) => x.name))
                    }}
                  >
                    {openProv.size === provRows.length ? 'Collapse all' : 'Expand all'}
                  </button>
                {/if}
              {/snippet}
              {#if !provRows.length}
                <div class="sempty" data-prov-empty>
                  No endpoint rows yet. The model answering now comes from
                  <span class="mono">config.toml</span>; discover it above and Add endpoint puts it
                  on this list without replacing anything.
                </div>
              {:else}
                <div class="slist">
                  {#each provRows as p (p.name)}
                    <div class="sitem">
                      <button class="sitem-h" aria-expanded={openProv.has(p.name)} onclick={() => toggleProv(p.name)}>
                        <span class="chev">{openProv.has(p.name) ? '\u25BE' : '\u25B8'}</span>
                        <span class="sitem-name">{p.name}</span>
                        {#if p.current}<span class="cap now">in use</span>{/if}
                        <code class="sitem-sub">{p.base_url}</code>
                        <span class="sitem-n">{p.models.length} model{p.models.length === 1 ? '' : 's'}</span>
                      </button>
                      {#if openProv.has(p.name)}
                        <div class="sitem-b">
                          <div class="srow">
                            <div class="srow-t">API compatibility</div>
                            <div class="srow-c mono">{p.kind}</div>
                          </div>
                          <div class="srow">
                            <div>
                              <div class="srow-t">API key</div>
                              <div class="srow-h">
                                {p.has_key
                                  ? 'stored in the config file; this screen never shows it back'
                                  : 'none: this endpoint takes none'}
                              </div>
                            </div>
                          </div>
                          {#each p.models as id (id)}
                            {@const m = capRows.find((x) => x.id === id)}
                            {@const d = draftFor(id)}
                            <div class="sitem">
                              <div class="sitem-top">
                                <button
                                  class="sitem-h"
                                  aria-expanded={openModel.has(id)}
                                  onclick={() => toggleModel(id)}
                                >
                                  <span class="chev">{openModel.has(id) ? '\u25BE' : '\u25B8'}</span>
                                  <span class="sitem-name mono">{id}</span>
                                  <span class="capset">
                                    {#each capLabel(m ?? { id }) as c (c)}<span class="cap">{c}</span>{/each}
                                  </span>
                                  {#if m?.current}<span class="sitem-n">in use</span>{/if}
                                </button>
                                {#if !m?.current}
                                  <button
                                    class="set-btn plain sitem-act"
                                    title="Answer with this model. The conversation keeps its folder and its history."
                                    onclick={() => void useModel(id)}
                                  >
                                    Use this
                                  </button>
                                {/if}
                              </div>
                              {#if openModel.has(id)}
                                <div class="sitem-b">
                                  <div class="sfld-grid">
                                    <Field
                                      label="Max context (tokens)"
                                      forId={`cw-mw-${id}`}
                                      hint="Stored here, this outranks what the endpoint claims and feeds the meter, the derived ceiling and the core's own fold."
                                    >
                                      <input
                                        class="sinp num mono"
                                        id={`cw-mw-${id}`}
                                        inputmode="numeric"
                                        bind:value={d.context_window}
                                        placeholder="ask the endpoint"
                                      />
                                    </Field>
                                    <Field label="Output cap (tokens)" forId={`cw-mt-${id}`} hint="The longest answer the endpoint is allowed to write.">
                                      <input
                                        class="sinp num mono"
                                        id={`cw-mt-${id}`}
                                        inputmode="numeric"
                                        bind:value={d.max_tokens}
                                        placeholder="the endpoint's own"
                                      />
                                    </Field>
                                    <Field label="Temperature" forId={`cw-temp-${id}`} hint="Empty uses the server's default.">
                                      <input class="sinp num" id={`cw-temp-${id}`} bind:value={d.temperature} placeholder="server default" />
                                    </Field>
                                    <Field label="top_p" forId={`cw-topp-${id}`} hint="Empty uses the server's default.">
                                      <input class="sinp num" id={`cw-topp-${id}`} bind:value={d.top_p} placeholder="server default" />
                                    </Field>
                                    <Field label="top_k" forId={`cw-topk-${id}`} hint="Empty uses the server's default.">
                                      <input class="sinp num" id={`cw-topk-${id}`} inputmode="numeric" bind:value={d.top_k} placeholder="server default" />
                                    </Field>
                                    <Field label="min_p" forId={`cw-minp-${id}`} hint="Empty uses the server's default.">
                                      <input class="sinp num" id={`cw-minp-${id}`} bind:value={d.min_p} placeholder="server default" />
                                    </Field>
                                    <Field label="Seed" forId={`cw-seed-${id}`} hint="A seed makes repeated answers comparable. Empty means unseeded.">
                                      <input class="sinp num" id={`cw-seed-${id}`} inputmode="numeric" bind:value={d.seed} placeholder="no seed" />
                                    </Field>
                                    <Field
                                      label="Thinking levels"
                                      forId={`cw-eff-${id}`}
                                      wide
                                      hint={`Comma-separated, in the order the endpoint takes them. ${m?.efforts?.length ? `This one answered: ${m.efforts.join(', ')}.` : 'Unknown until a discovery asks this endpoint what it accepts.'}`}
                                    >
                                      <input
                                        class="sinp"
                                        id={`cw-eff-${id}`}
                                        bind:value={d.efforts}
                                        placeholder="off,low,medium,high"
                                      />
                                    </Field>
                                  </div>
                                  <p class="set-note">
                                    {#if m?.source}
                                      What is stored here came from: {m.source}
                                    {:else}
                                      Nothing is stored for this model yet: its window comes from the
                                      endpoint each time the app starts.
                                    {/if}
                                  </p>
                                  <div class="pvacts">
                                    <button class="set-btn primary" onclick={() => void saveModel(id)}>Save</button>
                                    <button
                                      class="set-btn plain"
                                      onclick={() => {
                                        delete mDraft[id]
                                        void loadProviders()
                                      }}
                                    >
                                      Revert
                                    </button>
                                    <button class="set-btn danger" onclick={() => void forgetModel(id)}>
                                      Forget this model's settings
                                    </button>
                                  </div>
                                </div>
                              {/if}
                            </div>
                          {:else}
                            <p class="set-note">
                              No models are filed under this endpoint row. Discover the endpoint and
                              add it, and the models come with it.
                            </p>
                          {/each}
                          <div class="pvacts">
                            <button class="set-btn danger" onclick={() => void removeProvider(p.name)}>
                              Remove endpoint row
                            </button>
                          </div>
                          <p class="set-note">
                            Removing the row leaves the stored model settings alone on purpose: a
                            window or a sampling value someone set is a decision, not debris.
                          </p>
                        </div>
                      {/if}
                    </div>
                  {/each}
                </div>
              {/if}
            </Section>

            <Section
              id="reasoning"
              title="Reasoning"
              desc="How much the model deliberates before it answers. This is the default new conversations start at; a conversation can be given its own level from its composer, and that choice belongs to that conversation alone. Levels are a request: an endpoint that does not implement one answers anyway, at whatever depth it has."
            >
              {@render reasoningDefaultBody()}
            </Section>

            <Section
              id="ceiling"
              title="Context ceiling"
              desc="The number this conversation is measured and folded against. It is a setting, kept in the config file, so a phone and the desktop agree instead of each inventing a window."
            >
              {@render ceilingRow()}
              <p class="set-note">
                A ceiling below 4096 tokens is refused in words: a prompt that small cannot hold the
                system prompt, the tool definitions and a tool result, and still leave room for an
                answer. One at or above the endpoint's window is refused too, because the server
                would cut the prompt before this meter ever filled. A model process that is already
                running keeps the threshold it started with; the next one reads what you set here.
              </p>
            </Section>
          {:else if settings.page === 'workspace'}
            <Section
              id="tools"
              title="Tools and capabilities"
              desc="What the coworker can do. Each tool is defined once, in the engine, and the sentence beside its name is the same text the model is handed - a copy written in this window would be a second prompt that drifts."
            >
              {#if personaNow === 'chat'}
                <div class="smsg" data-chat-note>
                  This conversation is in Chat mode: none of these tools are mounted, and the model
                  answers with nothing attached. Switch the conversation to Cowork to use them.
                </div>
              {/if}
              {#if toolsErr}
                <div class="smsg bad" data-tools-error>{toolsErr}</div>
              {/if}
              <div class="toolsec">
                {#each toolViews as t (t.name)}
                  <div class="toolrow">
                    <code class="toolname">{t.name}</code>
                    <p class="tooldesc">{toolBlurb(t.description)}</p>
                  </div>
                {:else}
                  <div class="smsg bad">
                    The engine reported no tools. That is a fault, not an empty list: check the app log.
                  </div>
                {/each}
              </div>
              <p class="set-note">
                Nothing here is switchable yet. Per-tool permission is not wired into either core, and
                a control that changed nothing would be worse than no control. What may run without
                asking is set below, and where files may be written is set by the folder.
              </p>
            </Section>

            <Section
              id="approvals"
              title="Approvals"
              desc="What may run without asking you first. This decides whether a call waits for a person; it never decides what a call is allowed to touch."
            >
              {@render approvalsBody()}
            </Section>

            <Section
              id="folders"
              title="Folders the tools may work in"
              desc="A conversation and its folder travel together: opening another conversation moves the working directory, and a turn that is already running keeps the folder it started in."
            >
              <div class="srow" data-ws-current>
                <div>
                  <div class="srow-t">Where this conversation's tools work</div>
                  <div class="srow-h mono">{hostState?.workspace || 'sessions with no folder'}</div>
                </div>
                <div class="srow-c">
                  <button
                    class="set-btn"
                    disabled={!hostState?.workspace}
                    onclick={() => newSessionIn(hostState?.workspace ?? '')}
                  >
                    New session here
                  </button>
                </div>
              </div>
              <Field
                label="Add a folder"
                forId="cw-ws-add"
                hint="Adding a folder to this list does not move the conversation you are in; choosing a conversation's folder does."
              >
                <div class="pvacts">
                  {#if transportKind === 'tauri'}
                    <button class="set-btn" onclick={() => void addFolderPicked()}>Choose a folder…</button>
                  {/if}
                  <input
                    class="sinp mono"
                    id="cw-ws-add"
                    type="text"
                    style="flex:1; min-width:16ch"
                    placeholder="/home/you/project"
                    bind:value={wsAddDraft}
                    onkeydown={(e) => e.key === 'Enter' && void addFolderTyped()}
                  />
                  <button class="set-btn plain" disabled={!wsAddDraft.trim()} onclick={() => void addFolderTyped()}>
                    Add path
                  </button>
                </div>
              </Field>
              {#if wsAddErr}
                <div class="smsg bad" data-ws-error>{wsAddErr}</div>
              {/if}
              <div class="slist" data-ws-list>
                {#each workspaces as w (w.path)}
                  {@const inside = wsInside[w.path] ?? -1}
                  <div class="sitem">
                    <button class="sitem-h" aria-expanded={wsOpen.has(w.path)} onclick={() => void toggleWsRow(w.path)}>
                      <span class="chev">{wsOpen.has(w.path) ? '\u25BE' : '\u25B8'}</span>
                      <span class="sitem-name">{wsLabelOf(w)}</span>
                      <code class="sitem-sub">{w.path}</code>
                      {#if hostState?.workspace === w.path}<span class="cap now">in use</span>{/if}
                      <span class="sitem-n">
                        {inside >= 0 ? `${inside} conversation${inside === 1 ? '' : 's'} inside` : ''}
                      </span>
                    </button>
                    {#if wsOpen.has(w.path)}
                      <div class="sitem-b">
                        {#if wsRenaming === w.path}
                          <Field
                            label="Label"
                            forId="cw-ws-label"
                            hint="Names the folder in the sidebar. The path on disk is never changed, and no conversation moves."
                          >
                            <input
                              class="sinp"
                              id="cw-ws-label"
                              bind:value={wsLabelDraft}
                              onkeydown={(e) => e.key === 'Enter' && void commitWsRename()}
                            />
                          </Field>
                          <div class="pvacts">
                            <button class="set-btn primary" onclick={() => void commitWsRename()}>Save label</button>
                            <button class="set-btn plain" onclick={() => (wsRenaming = '')}>Cancel</button>
                          </div>
                        {:else}
                          <div class="pvacts">
                            <button class="set-btn" onclick={() => newSessionIn(w.path)}>New session here</button>
                            <button class="set-btn plain" onclick={() => startWsRename(w.path, w.label)}>Rename</button>
                            {#if gatewayRole() !== 'watch'}
                              <button class="set-btn plain" onclick={() => void removeFolder(w.path)}>
                                Take off the sidebar
                              </button>
                              <button
                                class="set-btn danger"
                                data-ws-delete={w.path}
                                onclick={() => void deleteFolderAndRows(w.path)}
                              >
                                {wsDeleteArm === w.path
                                  ? 'Press again to delete for good'
                                  : inside > 0
                                    ? `Delete folder and its ${inside} conversation${inside === 1 ? '' : 's'}`
                                    : 'Delete folder'}
                              </button>
                            {/if}
                          </div>
                          <p class="set-note">
                            {inside > 0
                              ? 'Delete removes every conversation filed under this path, including chat conversations and empty drafts that the sidebar list does not show. A conversation mid-turn is left alone and the folder stays while anything remains.'
                              : 'Nothing is filed here, so deleting it drops the row and nothing else.'}
                          </p>
                          {#if gatewayRole() === 'watch'}
                            <p class="set-note">
                              This window is attached as a watcher: it can read this list but cannot
                              change it. Connect with the drive token to make changes.
                            </p>
                          {/if}
                        {/if}
                      </div>
                    {/if}
                  </div>
                {:else}
                  <div class="sempty">
                    No folders are registered. The tools work in the folder shown above.
                  </div>
                {/each}
              </div>
            </Section>

            <Section
              id="search"
              title="Search and web access"
              desc="How web_search and web_fetch reach outside this machine, and what happens when no search instance is running."
            >
              {@render webBody()}
            </Section>
          {:else if settings.page === 'devices'}
            <Section
              id="gateway"
              title="Connected devices"
              desc="Other windows, phones and machines that attach to this app over the network, and the address they attach to."
            >
              {@render devicesBody()}
            </Section>
          {:else if settings.page === 'conversation'}
            <Section
              id="titles"
              title="Conversation titles"
              desc="How a conversation gets its name, and what its first answer waits for."
            >
              <p class="set-note" data-titles-explained>
                A new conversation starts out called <span class="mono">New Session</span>. Before it
                answers anything, the model behind this app is asked, in one separate request of its
                own: <span class="mono">Title this conversation in under ~10 words and say nothing
                more: …</span> — no system prompt, no tools, thinking off. The name types itself into
                the sidebar as it arrives, and the answer starts after it. The stall is the trade: a
                name that appears forty seconds late, on a page that has already moved on, is not a
                name.
              </p>
              <div class="srow">
                <div>
                  <div class="srow-t">One short request, paid for once per conversation</div>
                  <div class="srow-h">
                    It is not part of the conversation. The title ask is never added to the history, so
                    it cannot inflate a later turn, and it cannot disturb what the server has cached of
                    the real prompt. There is no second model and nothing to install or download:
                    the conversation is named by whatever is already answering it.
                  </div>
                </div>
              </div>
              <div class="srow">
                <div>
                  <div class="srow-t">If the endpoint gives no usable name</div>
                  <div class="srow-h" data-titles-fallback>
                    The row falls back to your opening line rather than sitting on
                    <span class="mono">New Session</span> forever — a sidebar of identical rows says
                    nothing — and the reason goes to the log. A model that thinks anyway, or answers
                    inside its thinking block, is parsed rather than rejected. A name you typed is
                    never overwritten, by anything.
                  </div>
                </div>
              </div>
            </Section>

            <Section
              id="session"
              title="This conversation"
              desc="What the conversation currently on screen has used: the context it carries, what the server already had cached, and how fast the answer arrived."
            >
              {#if !lastUsage && !turnMs}
                <div class="smsg wait">
                  Nothing measured in this window yet. These numbers appear while the first turn
                  streams and stay for the rest of the session.
                </div>
              {/if}
              {@render sessionReadouts(false)}
              <div class="srow">
                <div>
                  <div class="srow-t">Context ceiling</div>
                  <div class="srow-h" data-budget-provenance>
                    {fmtTokens(hostState?.context_budget ?? 0)} tokens ·
                    {hostState?.budget_pin
                      ? `pinned at ${fmtTokens(hostState.budget_pin)}`
                      : 'derived from the endpoint window'}
                    - the setting itself is under Models
                  </div>
                </div>
                <div class="srow-c">
                  <button class="set-btn plain" onclick={() => openSettings('ceiling')}>
                    Change the ceiling
                  </button>
                </div>
              </div>
            </Section>
          {:else if settings.page === 'appearance'}
            <Section
              id="transcript"
              title="Transcript"
              desc="How much of the working the conversation shows: the thinking and tool steps a turn took are folded into one run, and this decides whether what they produced is folded too."
            >
              {#each uiPrefs.filter((p) => p.group === 'transcript') as p (p.key)}
                <div class="srow" data-pref={p.key}>
                  <div>
                    <div class="srow-t" data-pref-label>{p.label}</div>
                    <div class="srow-h">{p.hint}</div>
                    {#if !p.is_default}
                      <div class="srow-h">set by you: {p.value} (the default is {p.default})</div>
                    {/if}
                  </div>
                  <div class="srow-c">
                    <Segmented
                      aria={p.label}
                      value={p.value}
                      options={p.values.map((v) => ({ value: v, label: v }))}
                      disabled={prefBusy}
                      onpick={(v) => void setPref(p.key, v)}
                    />
                  </div>
                </div>
              {/each}
              <p class="set-note">
                This list is the host's, not this window's: a setting the app does not act on never
                appears here, and it is stored in the app's own database, so the desktop, a browser on
                another port and a phone read one answer. None of it changes what the model is told.
              </p>
            </Section>

            <Section
              id="theme"
              title="Theme"
              desc="The palette of the window you are looking at."
            >
              <div class="srow">
                <div>
                  <div class="srow-t">Colours</div>
                  <div class="srow-h">
                    Kept per window: a desktop and a browser opened against the same app can disagree,
                    because this is a fact about the screen rather than about the conversation. With no
                    choice saved, the app follows what the system asks for.
                  </div>
                </div>
                <div class="srow-c">
                  <Segmented
                    aria="Theme"
                    value={theme}
                    options={[{ value: 'light', label: 'Light' }, { value: 'dark', label: 'Dark' }]}
                    onpick={(v) => applyTheme(v === 'dark' ? 'dark' : 'light')}
                  />
                </div>
              </div>
            </Section>

            <Section
              id="shortcuts"
              title="Keyboard shortcuts"
              desc="The keys this app answers. They are the same in the desktop window and in a browser attached to it."
            >
              {@render keysBody()}
            </Section>
          {/if}
        </div>
      </div>
    </div>
  {/if}

  <!-- ========================== telemetry bar ========================= -->
  <!-- There is no bottom toolbar any more.

       It used to hold eight readouts and three menus, and it cost a 36px band across both the
       transcript and the sidebar - the two places that actually need the vertical room. The three
       things it was for (telemetry, gateway, theme) now live as icons at the foot of the sidebar,
       and everything else it carried - the search provider, the IPC round-trip, the `?` shortcut
       sheet - is in Settings, where it already was. A fact with nowhere honest to live is a fact
       that belongs in a tooltip or nowhere, not in a bar nobody reads.
  -->

  <!-- Refusals and outcomes that used to be invisible: a folder the engine declined
       while its panel was closed, a picker cancelled, a session whose folder cannot
       change. `role=status` so a screen reader hears it, and dismissible by hand. -->
  {#if toast}
    <div class="toast {toast.bad ? 'bad' : ''} {toast.note ? 'note' : ''}" role="status" aria-live="polite">
      <span>{toast.text}</span>
      <button class="toast-x" aria-label="Dismiss" onclick={() => (toast = null)}>
        <svg width="10" height="10" viewBox="0 0 10 10"><path d="M2.6 2.6l4.8 4.8M7.4 2.6l-4.8 4.8" stroke="currentColor" stroke-width="1.4" fill="none" stroke-linecap="round"/></svg>
      </button>
    </div>
  {/if}
</div>
