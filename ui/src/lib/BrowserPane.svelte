<script lang="ts">
  // The browser column. Two kinds of view, and the panel draws the one the app actually gave it:
  //
  // **the page in the window** (`panel.native`) — the page is a real page, painted by this app into
  // the box below, and this column is the frame around it. There is no picture of it, and a panel
  // that asked for one would be rendering a second copy of a page the person is already looking at.
  // The person clicks, types and scrolls the page itself; the model drives the same page through the
  // same hub. What the column owns is the box: it measures the area and tells the app where it is.
  //
  // **a window onto another browser** — the browser lives in its own process and pushes pictures of
  // itself; they arrive here, and every click, key and scroll goes back. That picture is the only
  // copy of that page this window can have, so it is interactive rather than a poster.
  //
  // Both share the address box, the history controls, the idle clock, the sentence the engine says,
  // and the drawer below — which is a drawer, not a permanent band: a page worth looking at does not
  // come with six rows of chrome underneath it.
  import Icon from './Icon.svelte'
  import { pagePoint as fitPagePoint } from './pagefit'
  import type { BrowserPanel, DiagRecord } from './events'

  let {
    panel,
    frame,
    frameAt = 0,
    frameAge,
    busy,
    err,
    wide = false,
    log = false,
    records = [],
    logErr = '',
    onopen,
    onact,
    onclose,
    onwide,
    onlog,
    onclosePane,
  }: {
    panel: BrowserPanel | null
    /** `data:` URL of the last picture, or null when there is none to show. */
    frame: string | null
    /** When the last picture arrived (ms). The panel says how old it is in both units. */
    frameAt?: number
    /** The same span in words, counted by the app that polls for pictures. */
    frameAge: string
    busy: boolean
    err: string
    /** Full window: the page is the screen, and the page is asked to take that shape. */
    wide?: boolean
    /** The drawer: the page's own log, open or shut. Lives in the app, because the app is the one
        that has the records and knows whether it is worth polling for them. */
    log?: boolean
    /** The newest records, when the drawer is open. */
    records?: DiagRecord[]
    logErr?: string
    onopen: (url: string) => void
    onact: (act: Record<string, unknown>) => void
    onclose: () => void
    onwide: () => void
    onlog: (open: boolean) => void
    onclosePane: () => void
  } = $props()

  let draft = $state('')
  let armed = $state(false)
  let keys = $state('')
  let dtab = $state<'log' | 'page' | 'about'>('log')
  let filter = $state<'' | 'console' | 'error' | 'network' | 'resource' | 'navigation' | 'download'>('')
  let shot = $state<HTMLImageElement | null>(null)
  let stage = $state<HTMLDivElement | null>(null)
  let addr = $state<HTMLInputElement | null>(null)
  let ripple = $state<{ left: number; top: number } | null>(null)
  let pointed = $state(false)
  // A press in the letterbox ground beside the page. It is not an error and it is not nothing: it is
  // the panel declining to click a page that is not there, and the person deserves to be told which.
  let beside = $state<{ left: number; top: number } | null>(null)
  let lastKeys = $state('')

  const alive = $derived(!!panel?.alive)
  const opened = $derived(alive && !!panel?.opened)
  const writable = $derived(!!panel?.writable)
  const keepOff = $derived(panel?.keep_off === true)
  /** The page is a page: this app painted it, into the box this component just measured. */
  const inPage = $derived(panel?.native === true)
  const url = $derived(panel?.url || '')
  const title = $derived(panel?.title || '')
  const secure = $derived(url.startsWith('https://'))
  const host = $derived(hostOf(url))
  const words = $derived(panel ? panel.says : 'asking the app about its browser…')
  const idleLeft = $derived(panel?.idle_left ?? null)
  const idleSecs = $derived(panel?.idle_secs || 300)
  const frac = $derived(idleLeft == null ? 0 : Math.max(0, Math.min(1, idleLeft / Math.max(1, idleSecs))))
  const mmss = $derived(clockWords(idleLeft))
  const lastAct = $derived(panel?.last_act || '')
  const lastActor = $derived(panel?.last_actor || 'nobody')
  const events = $derived(panel?.console ?? [])
  const pageErrs = $derived(panel?.page_errors ?? [])
  const failedReqs = $derived(panel?.requests_failed ?? [])
  const blocked = $derived(panel?.downloads_blocked ?? [])
  const trouble = $derived(pageErrs.length + failedReqs.length + blocked.length)
  const noticeCount = $derived(trouble + events.length)
  /** Nothing open, nothing done, nothing worth explaining: the panel's one-line sentence is then
      "no browser is open." said a second time, under a page area that already says start browsing.
      The row stays for the shapes where it carries news — a browser that is running, the last thing
      either of you did to it, a close that had a reason, or a kept browser that is switched off. */
  const quiet = $derived(
    !alive &&
      !lastAct &&
      !(panel?.why ?? '').trim() &&
      !keepOff &&
      // ...and only when a browser exists to be absent. A start that mounted none has something to
      // say, and the sentence is the news rather than a repeat of the shape around it.
      panel?.mounted !== false,
  )
  /* The page's own record, as numbers — the same numbers the drawer is built from, so a badge that
     says 3 is a drawer that shows 3. */
  const diagCounts = $derived(panel?.diag_counts ?? {})
  const diagFailed = $derived(panel?.diag_failed ?? 0)
  const diagSeq = $derived(panel?.diag_seq ?? 0)
  const diagTotal = $derived(Object.values(diagCounts).reduce((a, b) => a + (Number(b) || 0), 0))
  const shown = $derived(records.filter((r) => !filter || r.kind === filter))
  const framesTaken = $derived(panel?.frames_taken ?? 0)
  const viewport = $derived(panel?.viewport ?? null)
  const canPoint = $derived(!inPage && writable && opened && !busy)
  /* Whether the *browser* is pushing frames, which the driver answers and this panel only repeats.
     Asking is not the same as getting: a Chromium without a screencast says so, and a panel that
     claimed "live" because it had asked would be making a promise the build cannot keep. For a page
     that is in the window there is no stream to be live: the page is the moving thing. */
  const live = $derived(alive && !inPage && panel?.live === true)
  const liveWhy = $derived(panel?.live_why || '')

  /* Where the widget tree actually put the page.

     For a page in this window the panel measures an empty area and the app paints a native widget
     over it. Those are two sides of a promise, and when they disagree the result is a bordered box
     with nothing in it — the one failure this column must not have, because an empty frame looks
     exactly like a page that failed to load. So the disagreement is said, in both sets of numbers. */
  let placedOff = $state('')
  let askedAt = Date.now()
  $effect(() => {
    placedOff = ''
    if (!inPage || !opened) return
    void panel
    const a = stage?.querySelector('[data-browser-pagearea]')?.getBoundingClientRect()
    if (!a || a.width < 4 || a.height < 4) return
    const asked = `${Math.round(a.width)}x${Math.round(a.height)} at ${Math.round(a.left)},${Math.round(a.top)}`
    const pl = panel?.placed
    if (!pl || pl.width < 4 || pl.height < 4) {
      placedOff =
        `this panel left ${asked} for the page, and the page was given no box at all — the page is not in that frame`
      return
    }
    const off = Math.max(
      Math.abs(pl.x - a.left),
      Math.abs(pl.y - a.top),
      Math.abs(pl.width - a.width),
      Math.abs(pl.height - a.height),
    )
    // The area is measured now and the box was reported as of the last placement, so a panel whose
    // own layout just changed (a page opening turns a placeholder into a page) is allowed to be one
    // poll behind. What is not allowed is a box that stays behind: a placement that has been asked
    // for and never happens is a page in the wrong rectangle, and this line is how it gets said.
    if (off > 140) {
      askedAt = Date.now()
      return
    }
    if (off > 8 && Date.now() - askedAt < 2600) return
    if (off > 8)
      placedOff =
        `the page widget is ${pl.width}x${pl.height} at ${pl.x},${pl.y}${pl.shown ? '' : ' and is not shown'}, ` +
        `and this panel asked for ${asked} — the page is not the box it was told about`
  })

  function hostOf(u: string): string {
    try {
      return new URL(u).host
    } catch {
      return u
    }
  }
  function clockWords(left: number | null): string {
    if (left == null) return ''
    const s = Math.max(0, Math.round(left))
    const m = Math.floor(s / 60)
    return m > 0 ? `${m}:${String(s % 60).padStart(2, '0')}` : `${s}s`
  }
  function ago(ms: number): string {
    if (!ms) return 'never'
    const s = Math.round((Date.now() - ms) / 1000)
    return s < 60 ? `${s}s ago` : `${Math.round(s / 60)}m ago`
  }

  function submit() {
    const u = draft.trim()
    if (!u) return
    onopen(u)
  }

  /** One kind per chip, in the order a person reads a page: what it said, what broke, what it asked
   *  for, what it moved to, what it handed over. A row of chips rather than a filter box because the
   *  kinds are known and finite, and a box that accepts any word invites a filter that shows nothing
   *  and looks like an empty log. */
  const chips: { k: typeof filter; word: string }[] = [
    { k: '', word: 'all' },
    { k: 'console', word: 'console' },
    { k: 'error', word: 'errors' },
    { k: 'network', word: 'requests' },
    { k: 'resource', word: 'resources' },
    { k: 'navigation', word: 'navigations' },
    { k: 'download', word: 'files' },
  ]

  function act(a: Record<string, unknown>) {
    armed = false
    onact(a)
  }

  function sendKeys() {
    const t = keys
    if (!t) return
    lastKeys = t
    keys = ''
    act({ do: 'type_keys', text: t })
  }

  /** The point in the page that a click on the picture stands for, in page pixels.
   *
   * The picture is the viewport at the size the browser reports, drawn to fit the stage box — which
   * is not the same rectangle when the two aspects differ, and `pagefit` is where that difference is
   * worked out. A click scaled by the element box instead of the drawn page lands in the ground
   * beside the page, on a page the person can only reach through this picture.
   */
  function pagePoint(e: { clientX: number; clientY: number }): { x: number; y: number } | null {
    const img = shot
    const vp = viewport
    if (!img || !vp || !vp.width || !vp.height) return null
    return fitPagePoint(img.getBoundingClientRect(), vp, e.clientX, e.clientY)
  }

  function onShotClick(e: MouseEvent) {
    if (!canPoint) return
    if (inPage) return
    const p = pagePoint(e)
    if (!p) {
      /* The press was in the ground beside the page, not on it. Saying nothing is the bad option:
         the person aimed at something, and a panel that ignores it looks broken rather than precise.
         The picture is drawn inside the box it is shown in, and where the aspects differ there is
         ground on two sides — so the panel names the ground it just refused to click. */
      beside = { left: e.clientX, top: e.clientY }
      setTimeout(() => (beside = null), 1600)
      return
    }
    const box = stage ?? shot
    if (box) {
      const r = box.getBoundingClientRect()
      ripple = { left: e.clientX - r.left, top: e.clientY - r.top }
      setTimeout(() => (ripple = null), 450)
    }
    pointed = true
    act({ do: 'click_xy', x: p.x, y: p.y })
  }

  // A wheel is gathered for ~140ms and sent once. A wheel arrives as a dozen small events, and a
  // page handed a dozen separate wheels takes a dozen separate steps, which reads as a jump.
  let wheelPx = 0
  let wheelAt = { x: 0, y: 0 }
  let wheelTimer: ReturnType<typeof setTimeout> | null = null
  function onShotWheel(e: WheelEvent) {
    if (inPage) return
    if (!writable || !opened) return
    e.preventDefault()
    const p = pagePoint(e) ?? { x: (viewport?.width ?? 0) / 2, y: (viewport?.height ?? 0) / 2 }
    wheelAt = p
    wheelPx += e.deltaY
    if (wheelTimer) return
    wheelTimer = setTimeout(() => {
      wheelTimer = null
      const px = Math.max(-4000, Math.min(4000, Math.round(wheelPx)))
      wheelPx = 0
      if (px) act({ do: 'scroll', px, x: wheelAt.x, y: wheelAt.y })
    }, 140)
  }

  function onPaneKey(e: KeyboardEvent) {
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'l') {
      e.preventDefault()
      addr?.focus()
      addr?.select()
      return
    }
    // Escape drops an armed confirmation before it does anything else. What it must never do here
    // is close the browser: the person who pressed Escape wanted out of a question, not out of a
    // browser they had pages open in. Out of a full-window page comes second: same key, and the
    // page underneath is where they were already standing.
    if (e.key === 'Escape' && armed) armed = false
    else if (e.key === 'Escape' && wide) onwide()
  }

  $effect(() => {
    if (!alive) armed = false
  })
</script>

<!-- the pane itself answers only to the keyboard: Escape for "leave that question alone",
     Cmd/Ctrl-L for "put the caret in the address box". The region is labelled, so these are
     shortcuts inside a landmark the person is already in, not a div pretending to be a control. -->
<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<div class="browserpane" data-browser-pane data-browser-open="yes" data-browser-alive={alive ? 'yes' : 'no'}
  data-browser-writable={writable ? 'yes' : 'no'}
  data-browser-live={live ? 'yes' : 'no'}
  data-browser-inpage={inPage ? 'yes' : 'no'}
  data-browser-wide={wide ? 'yes' : 'no'}
  data-browser-log-open={log ? 'yes' : 'no'}
  data-browser-dtab={dtab}
  role="region" aria-label="cwoffee's kept browser" onkeydown={onPaneKey}>
  <header class="bpy-bar">
    <span class="bpy-glyph" class:live={alive} aria-hidden="true"><Icon name="browser" size={15} decorative /></span>
    <div class="bpy-id">
      <div class="bpy-title" data-browser-title={title || ''} title={title || words}>
        {title || (opened ? 'a page with no title' : 'Browser')}
      </div>
      <div class="bpy-state">
        <span class="bpy-pip" class:live={alive} class:off={keepOff} aria-hidden="true"></span>
        {#if keepOff}
          <span>switched off in settings</span>
        {:else if !panel}
          <span>connecting…</span>
        {:else if !alive}
          <span>closed</span>
        {:else if opened}
          <span>{inPage ? 'the page itself' : live ? 'live' : 'still'}{inPage ? '' : ` · ${framesTaken} picture${framesTaken === 1 ? '' : 's'}`}</span>
        {:else}
          <span>ready · nothing open</span>
        {/if}
        {#if trouble}
          <button class="bpy-trouble" type="button" aria-pressed={log}
            onclick={() => onlog(!log)} title="{trouble} thing(s) the page reported about itself">
            <Icon name="alert" size={11} decorative />{trouble}
          </button>
        {/if}
      </div>
    </div>
    {#if idleLeft != null}
      <div class="bpy-clock" data-browser-clock title="closes by itself when nobody uses it">
        <svg class="bpy-ring" viewBox="0 0 22 22" aria-hidden="true">
          <circle class="bpy-ring-b" cx="11" cy="11" r="8.5"></circle>
          <circle class="bpy-ring-f" cx="11" cy="11" r="8.5" stroke-dasharray="53.4"
            stroke-dashoffset={53.4 * (1 - frac)}></circle>
        </svg>
        <span class="bpy-clock-n">{mmss}</span>
        <span class="ic-sr">closes in {Math.floor(idleSecs / 60)}m{String(idleSecs % 60).padStart(2, '0')}s —
          {idleSecs} seconds with nobody touching it</span>
      </div>
    {/if}
    <button class="bpy-ic" type="button" data-browser-logbtn aria-pressed={log}
      aria-label={log ? 'Put the page’s own log away' : 'Show the page’s own log'}
      title={log
        ? 'Away. The page is still watched — closing this only stops the panel from asking.'
        : `What the page said, threw and failed to load${diagTotal ? ` (${diagTotal})` : ''}. It is collected the whole time the page is open; this box is only whether you are looking at it.`}
      onclick={() => onlog(!log)}>
      <Icon name="activity" size={15} decorative />
      {#if diagFailed || trouble}<span class="bpy-badge" class:bad={diagFailed > 0 || trouble > 0}>{diagFailed || trouble}</span>{/if}
    </button>
    <button class="bpy-ic" type="button" data-browser-wide-btn aria-pressed={wide ? 'true' : 'false'}
      aria-label={wide ? 'Give the page its column back' : 'Give the page the whole window'}
      title={wide
        ? 'Back to the column. Escape does the same, and the page keeps its size.'
        : 'The page becomes the window: the whole screen, and the page is asked to fill it.'}
      onclick={onwide}>
      <Icon name={wide ? 'shrink' : 'expand'} size={15} decorative />
    </button>
    <button class="bpy-ic" type="button" data-browser-close-pane aria-label="Hide the browser panel"
      title="Hide this panel. The browser stays open and keeps its idle clock." onclick={onclosePane}>
      <Icon name="x" size={15} decorative />
    </button>
  </header>

  <div class="bpy-nav">
    <button class="bpy-ic" type="button" data-browser-back aria-label="Back one page"
      title="Back: one step in this browser's own history. It does not reload the page."
      disabled={!alive || busy} onclick={() => act({ do: 'back' })}>
      <Icon name="back" size={15} decorative />
    </button>
    <button class="bpy-ic" type="button" data-browser-forward aria-label="Forward one page"
      title="Forward: one step along this browser's own history." disabled={!alive || busy}
      onclick={() => act({ do: 'forward' })}>
      <Icon name="forward" size={15} decorative />
    </button>
    <button class="bpy-ic" type="button" data-browser-reload aria-label="Reload this page"
      title="Reload: re-reads the page that is open. It does not go back." disabled={!opened || busy}
      onclick={() => act({ do: 'reload' })}>
      <span class="bpy-spin" class:spinning={busy} aria-hidden="true"><Icon name="reload" size={15} decorative /></span>
    </button>
    <button class="bpy-ic" type="button" data-browser-stop aria-label="Stop loading this page"
      title="Stop: the page stops fetching. What has arrived so far stays."
      disabled={!busy} onclick={() => act({ do: 'stop' })}>
      <Icon name="stop" size={15} decorative />
    </button>
    <label class="bpy-addr" title={writable ? '' : 'this window can watch this browser; the machine running the tools drives it'}>
      <span class="bpy-sec" title={secure ? 'https' : 'not https'} aria-hidden="true">
        <Icon name={secure ? 'lock' : 'globe'} size={13} decorative />
      </span>
      <!-- A box that can never work is worse than a disabled one that says why. The engine refuses a
           write from a watcher anyway; the point of disabling is that the reason arrives before the
           attempt rather than after it. -->
      <input class="bpy-urlin" data-browser-urlbox type="text" spellcheck="false" autocomplete="off" inputmode="url"
        placeholder={writable ? 'type an address and press Enter (⌘L)' : 'this window only watches the browser'}
        bind:value={draft} disabled={!writable}
        aria-label="Address to open in cwoffee's browser"
        onkeydown={(e) => e.key === 'Enter' && submit()} />
      <button class="bpy-ic bpy-go" type="button" data-browser-go aria-label="Open this address"
        title="Opens in the kept browser. A card names the site it will visit first."
        disabled={!writable || !draft.trim() || busy} onclick={submit}>
        <Icon name="send" size={15} decorative />
      </button>
    </label>
  </div>

  {#if err}
    <div class="bpy-err" data-browser-err role="alert">
      <Icon name="alert" size={13} decorative />
      <span>{err}</span>
    </div>
  {/if}

  <div class="bpy-stage" bind:this={stage} data-browser-stage aria-busy={busy}
    class:wait={alive && !inPage && !frame} class:pointable={canPoint} class:live={live}
    class:inpage={inPage && opened} class:quiet={!alive && !inPage}>
    {#if inPage}
      <!-- The page is painted by this app over the area below, so there is nothing to draw here and
           nothing to click: whatever appears in that area is the page, with its own links, its own
           caret, its own scroll. The area is empty on purpose — a placeholder here would be a panel
           guessing at what the page looks like, which is the thing this column stopped doing. The
           caption under it is the panel's own line and stays visible, because the page covers the
           area and not the words beside it. -->
      <div class="bpy-pagearea" data-browser-pagearea aria-hidden="true"></div>
      {#if placedOff}
        <div class="bpy-placedoff" data-browser-placedoff>
          <Icon name="alert" size={11} decorative />
          <span>{placedOff}</span>
        </div>
      {/if}
      {#if opened}
        <div class="bpy-pagecap" data-browser-pagecap>
          <Icon name="browser" size={11} decorative />
          the page is here: click it, type in it, scroll it — and the model reads and drives this same page
        </div>
      {/if}
    {:else if frame && opened}
      <!-- the picture is a picture: it is the page's own pixels, sent by the browser, and clicking it
           sends a click to those pixels rather than to this window -->
      <!-- svelte-ignore a11y_click_events_have_key_events -->
      <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
      <img class="bpy-shot" data-browser-frame="yes" src={frame} bind:this={shot}
        alt="Picture of {host || 'the open page'} — what cwoffee's browser last saw"
        class:live={canPoint} onclick={onShotClick} onwheel={onShotWheel} />
      {#if ripple}
        <span class="bpy-ripple" style="left:{ripple.left}px;top:{ripple.top}px" aria-hidden="true"></span>
      {/if}
      {#if beside}
        <!-- The ground beside the page, named. The alternative is a panel that appears to ignore a click. -->
        <span class="bpy-beside" data-browser-beside style="left:{beside.left - (stage?.getBoundingClientRect().left ?? 0)}px;top:{beside.top - (stage?.getBoundingClientRect().top ?? 0)}px" role="status">that is the ground beside the page, not the page</span>
      {/if}
      {#if canPoint && !pointed}
        <span class="bpy-hint" aria-hidden="true">
          <Icon name="pointer" size={11} decorative />click the picture to click the page
        </span>
      {/if}
      {#if live && opened}
        <!-- The live view's own mark: the page is moving on its own, and this is the mark that says
             the picture on the screen is not a photograph of a moment. -->
        <span class="bpy-live" data-browser-live-mark aria-hidden="true"><i></i>live</span>
      {/if}
    {:else if alive}
      <div class="bpy-skel" data-browser-frame="waiting">
        <span class="bpy-skel-i" aria-hidden="true"><Icon name="browser" size={26} decorative /></span>
        <span class="bpy-skel-t">waiting for the first picture</span>
      </div>
    {:else}
      <div class="bpy-empty" data-browser-frame="none" data-browser-empty={keepOff ? 'off' : 'nothing'}>
        {#if keepOff}
          <span class="bpy-empty-i" aria-hidden="true"><Icon name="plug" size={30} decorative /></span>
          <div class="bpy-empty-t">Kept browsers are switched off</div>
          <div class="bpy-empty-w">{panel?.off_reason || 'turn kept browsers on in settings'}</div>
        {:else if panel?.mounted === false}
          <!-- A column that cannot browse says so rather than inviting a start it would refuse. The
               invitation is only honest where typing an address would work. -->
          <span class="bpy-empty-i" aria-hidden="true"><Icon name="plug" size={30} decorative /></span>
          <div class="bpy-empty-t">No browser in this start</div>
          <div class="bpy-empty-w">{panel?.route || 'this start mounted no kept browser'}</div>
        {:else}
          <!-- Nothing open is an invitation, not a diagnosis. This used to say "No browser is open"
               over the outline of a page — true, and useless: the shape already said it, and a
               sentence about what is *missing* makes a starting point read as a failure. The
               browser's own mark, then what to do here, then the other way in. The address field is
               one press away at the top of this column, so the second line names the other door
               rather than repeating the first. -->
          <span class="bpy-empty-i" data-browser-empty-glyph aria-hidden="true">
            <Icon name="browser" size={30} decorative />
          </span>
          <div class="bpy-empty-t" data-browser-empty-cta>Start browsing</div>
          <div class="bpy-empty-w" data-browser-empty-sub>...or ask cwoffee to browse for you.</div>
        {/if}
      </div>
    {/if}
    {#if busy}<span class="bpy-busybar" aria-hidden="true"></span>{/if}
  </div>

  <div class="bpy-keys" class:gone={!writable || !alive || inPage}>
    <span class="bpy-keys-i" aria-hidden="true"><Icon name="keyboard" size={13} decorative /></span>
    <input class="bpy-keys-in" data-browser-keys type="text" enterkeyhint="send"
      placeholder="keys go to the page — click a field in the picture first"
      aria-label="Type into the page that is open" bind:value={keys} disabled={!opened || busy}
      onkeydown={(e) => {
        if (e.key === 'Enter') {
          e.preventDefault()
          sendKeys()
        }
      }} />
    <button class="bpy-ic" type="button" aria-label="Press Enter in the page" title="Enter, over the page"
      disabled={!opened || busy} onclick={() => act({ do: 'press', key: 'Enter' })}>
      <Icon name="send" size={13} decorative />
    </button>
  </div>

  {#if url}
    <div class="bpy-crumb">
      <span class="bpy-host" title={url}>{host}</span>
      <span class="bpy-url" data-browser-url title={url}>{url}</span>
    </div>
  {/if}
  <!-- With nothing open this row used to read "no page is open" — a third way of saying what the
       page area and the engine's own sentence already say, three lines apart. A row that repeats is
       noise that pushes the one sentence worth reading further down. -->

  {#if !quiet}
    <div class="bpy-status" aria-live="polite">
      <span class="bpy-says" data-browser-says title={words}>{words}</span>
      {#if lastAct}
        <span class="bpy-act" data-browser-actor={lastActor} title="the last thing done in this browser, and by whom">
          <Icon name={lastActor === 'person' ? 'person' : 'model'} size={11} decorative />
          {lastAct}
        </span>
      {/if}
    </div>
  {/if}

  <!-- One drawer, shut by default. A page worth looking at does not come with a band of chrome
     underneath it: what the page reported, what broke and what this window knows about the session
     are here, opened by the button in the header that carries the count. Closing the drawer does not
     stop the page being watched — the records keep being collected, and the model can read them
     whether or not anybody is looking at this box. -->
  {#if log}
    <div class="bpy-drawer" data-browser-drawer role="region" aria-label="What the page has been reporting">
      <div class="bpy-dbar">
        <div class="bpy-tabs" role="group" aria-label="Which of them">
          <button class="bpy-tab" type="button" data-browser-dtabbtn="log" aria-pressed={dtab === 'log'}
            onclick={() => (dtab = 'log')}>
            <Icon name="activity" size={13} decorative />The page's own log
            {#if diagTotal || noticeCount}<span class="bpy-badge" class:bad={diagFailed > 0 || trouble > 0}>{diagFailed || diagTotal || noticeCount}</span>{/if}
          </button>
          <button class="bpy-tab" type="button" data-browser-dtabbtn="page" aria-pressed={dtab === 'page'}
            onclick={() => (dtab = 'page')}>
            <Icon name="page" size={13} decorative />This step
          </button>
          <button class="bpy-tab" type="button" data-browser-dtabbtn="about" aria-pressed={dtab === 'about'}
            onclick={() => (dtab = 'about')}>
            <Icon name="info" size={13} decorative />About this browser
          </button>
        </div>
        <button class="bpy-ic" type="button" data-browser-logclose aria-label="Put the log away"
          title="Away. The page is still watched; this only stops this box from asking." onclick={() => onlog(false)}>
          <Icon name="x" size={13} decorative />
        </button>
      </div>

      {#if dtab === 'log'}
        <div class="bpy-chips" role="group" aria-label="Which records to show">
          {#each chips as c (c.k)}
            <button class="bpy-chip" type="button" data-browser-chip={c.k || 'all'} aria-pressed={filter === c.k}
              onclick={() => (filter = c.k)}>
              {c.word}
              {#if c.k && diagCounts[c.k]}<span class="bpy-n">{diagCounts[c.k]}</span>{/if}
            </button>
          {/each}
          <span class="bpy-dsum" data-browser-diagsum>
            {diagTotal} record{diagTotal === 1 ? '' : 's'}
            {#if diagFailed}· {diagFailed} that went wrong{/if}
            {#if diagSeq}· newest #{diagSeq}{/if}
          </span>
        </div>
      {/if}

      <div class="bpy-body" data-browser-dbody>
        {#if dtab === 'log'}
          <div class="bpy-rows" data-browser-log>
            {#if shown.length}
              <!-- Oldest first: a log read downward is a story, and the newest thing at the bottom is
                   where the eye already is. `seq` is the page's own id, so a row can be quoted back to
                   the model by number. -->
              {#each shown.slice().reverse() as r (r.seq)}
                <div class="bpy-row" class:bad={r.level === 'error'} data-browser-diag={r.seq}>
                  <span class="bpy-kind" data-browser-diagkind={r.kind}>{r.kind}</span>
                  <span class="mono">
                    {r.text}
                    {#if r.url}<span class="bpy-durl" title={r.url}>{r.url}</span>{/if}
                    {#if r.status}<span class="bpy-dcode" data-browser-diagstatus={String(r.status)}>{r.status}</span>{/if}
                    {#if r.method}<span class="bpy-dverb">{r.method}</span>{/if}
                    {#if r.at}<span class="bpy-dat">{r.at}</span>{/if}
                  </span>
                </div>
              {/each}
            {:else if logErr}
              <div class="bpy-row bad"><span class="mono">{logErr}</span></div>
            {:else if filter}
              <div class="bpy-dim">Nothing of that kind. {diagTotal ? `The page has made ${diagTotal} record${diagTotal === 1 ? '' : 's'} in all.` : 'Nothing at all so far.'}</div>
            {:else if !opened}
              <div class="bpy-dim">No page has been open in this window, so nothing has been reported.</div>
            {:else}
              <div class="bpy-dim">Nothing yet. This page has said nothing and failed at nothing — which is its own answer.</div>
            {/if}
            {#if !diagTotal && (events.length || pageErrs.length || failedReqs.length || blocked.length)}
              <!-- A driver that speaks the older flat form (a browser in its own process, which sends
                   lines rather than records) still fills this same drawer. Two shapes of the same
                   sentence, and a person should not have to know which one is running. -->
              <div class="bpy-group">
                <div class="bpy-gh">from the driver, in its own words</div>
                {#each pageErrs as e, i (i)}<div class="bpy-row bad"><span class="mono">{e}</span></div>{/each}
                {#each failedReqs as e, i (i)}<div class="bpy-row"><span class="mono">{e}</span></div>{/each}
                {#each blocked as e, i (i)}<div class="bpy-row"><span class="mono">{e}</span></div>{/each}
                {#each events.slice(-40) as e, i (i)}
                  <div class="bpy-row" class:bad={e.kind === 'error'}><span class="mono">{e.text}</span></div>
                {/each}
              </div>
            {/if}
          </div>
        {:else if dtab === 'page'}
          <div class="bpy-rows" data-browser-pageinfo>
            <div class="bpy-row"><span class="bpy-k">Last step</span><span>{lastAct || 'nothing yet'}</span></div>
            {#if lastKeys}
              <div class="bpy-row"><span class="bpy-k">You typed</span><span class="mono">{lastKeys}</span></div>
            {/if}
            {#if inPage}
              <div class="bpy-row"><span class="bpy-k">Where it is</span><span>this window, in the box above — not a picture of it</span></div>
            {:else}
              <div class="bpy-row">
                <span class="bpy-k">Picture</span>
                <span>{frame ? `taken ${ago(frameAt)}` : 'none yet'} · {framesTaken} taken{live ? ' · live' : ''}</span>
              </div>
            {/if}
            {#if viewport && viewport.width}
              <div class="bpy-row"><span class="bpy-k">Page size</span><span>{viewport.width}×{viewport.height}</span></div>
            {/if}
            {#if blocked.length}
              <div class="bpy-row bad">
                <span class="bpy-k"><Icon name="download" size={11} decorative />Files</span>
                <span>{blocked.length} file(s) the browser handed to the workspace instead of keeping: {blocked[0]}</span>
              </div>
            {/if}
            {#if diagFailed}
              <div class="bpy-row bad">
                <span class="bpy-k"><Icon name="alert" size={11} decorative />Page</span>
                <span>{diagFailed} thing(s) went wrong — the log has the words</span>
              </div>
            {/if}
            {#if !lastAct && !opened}
              <div class="bpy-dim">
                Nothing has happened in this browser yet.
                {writable ? 'Type an address, or ask the assistant to open one.' : 'The assistant can open one.'}
              </div>
            {/if}
          </div>
        {:else}
          <div class="bpy-rows">
            <div class="bpy-row"><span class="bpy-k">Which browser</span><span>{panel?.route || 'this window'}</span></div>
            {#if !inPage}
              <div class="bpy-row"><span class="bpy-k">Driver</span><span class="mono">{panel?.driver || 'not installed'}</span></div>
            {/if}
            <div class="bpy-row">
              <span class="bpy-k">View</span>
              <span data-browser-view={inPage ? 'in page' : live ? 'live' : 'still'}>
                {inPage
                  ? 'the page itself, painted by this app — there is no picture of it, and no second copy'
                  : live
                    ? 'live — the browser sends a frame whenever the page repaints'
                    : liveWhy
                      ? `a picture at a time (${liveWhy})`
                      : 'a picture at a time'}
              </span>
            </div>
            <div class="bpy-row">
              <span class="bpy-k">Idle clock</span>
              <span>{panel?.idle_secs != null ? `closes after ${panel.idle_secs} seconds with nobody touching it` : 'unknown'}</span>
            </div>
            <div class="bpy-row">
              <span class="bpy-k">Who drives</span>
              <span>{writable ? 'this window, and the assistant in the same page' : 'the machine running the tools; this window only watches'}</span>
            </div>
            <div class="bpy-note">{words}</div>
            <div class="bpy-dim">
              {#if inPage}
                The page in the box is the page: the same document, the same cookie jar, the same
                caret. A click in it is a click on the page, not on a picture of one, and every step the
                assistant takes lands in front of you as it happens. It is rendered by this app's own
                engine, which is why there are no screenshots of it — the model reads the page's words,
                its elements and its own record of what failed instead.
              {:else}
                The picture is the browser's own pixels, pushed out of the browser process — not a second
                page rendered here. This column's address box is not a website's own box, and a page that
                animates with nothing acting on it still arrives, because the frame comes from the repaint
                rather than from a command. Every click, key and scroll goes to the same browser the
                assistant works in — which is why opening a page here asks you first.
              {/if}
            </div>
          </div>
        {/if}
      </div>
    </div>
  {/if}
  <div class="bpy-foot">
    <button class="bpy-ic bpy-close" type="button" data-browser-close-browser aria-pressed={armed ? 'true' : 'false'}
      disabled={!alive} aria-label={armed ? 'Close the browser now, for certain' : 'Close the browser'}
      title={armed ? 'Press again to close it now, or Escape to leave it alone' : 'Closes the kept browser now — the next open starts a fresh one'}
      onclick={() => {
        if (armed) {
          armed = false
          onclose()
        } else {
          armed = true
        }
      }}>
      <Icon name={armed ? 'check' : 'power'} size={14} decorative />
      {#if armed}<span class="bpy-armed-w">close now</span>{/if}
    </button>
    <span class="bpy-foot-w">
      {#if alive}
        <!-- The clock is already in the header as a ring with the number on it. Saying "132s of quiet
             left" again here is the same fact twice, and the space it takes is the space the page
             wanted. What is worth a line here is only what the ring cannot say: that this browser
             does not time out, if it does not. -->
        {#if idleLeft == null}this browser does not time out{/if}
      {:else}
        {writable ? 'opens when you or the assistant opens a page' : 'opens when the assistant opens a page — this window cannot start one'}
      {/if}
    </span>
  </div>
</div>

<style>
  /* Scoped to the column, but the panel's shape is the window's shape: every colour here is a token,
     so both palettes grade the same way and nothing has to be remembered twice. */
</style>
