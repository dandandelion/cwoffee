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
  let thinking = $state<'off' | 'low' | 'medium' | 'high'>('low')
  let hostState = $state<HostState | null>(null)

  let sessions = $state<Session[]>([])
  let query = $state('')
  let hits = $state<SearchHit[]>([])
  let searchErr = $state('')

  let approval = $state<{ id: number; tool: string; risk: string; summary: string } | null>(null)

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
  function handle(ev: EngineEvent) {
    transcript?.apply(ev)
    switch (ev.kind) {
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
        break
      }
      case 'turnEnd':
        transcript?.flushNow()
        busy = false
        approval = null
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
      }
    } catch (e) {
      ipc = { ok: false, msg: String(e).slice(0, 140) }
    }
  }

  async function loadSessions() {
    try {
      sessions = await invoke<Session[]>('list_sessions')
    } catch (e) {
      pageLog(`list_sessions failed: ${e}`)
    }
  }

  async function runSearch() {
    const q = query.trim()
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

  async function openSession(id: number) {
    try {
      const msgs = await invoke<any[]>('session_messages', { id })
      transcript?.loadHistory(msgs)
      await invoke('switch_session', { id })
      await refreshState()
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

  async function setMode(m: string) {
    mode = m as any
    await invoke('set_mode', { mode: m }).catch((e) => pageLog(`set_mode: ${e}`))
  }
  async function setThinking(l: string) {
    thinking = l as any
    await invoke('set_reasoning', { level: l }).catch((e) => pageLog(`set_reasoning: ${e}`))
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
  async function closeWin() { await win?.close() }
  async function minimize() { await win?.minimize() }
  async function toggleMax() {
    if (!win) return
    const m = await win.isMaximized()
    if (m) await win.unmaximize()
    else await win.maximize()
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
  })

  onDestroy(stopFrames)

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
    <aside
      class="hidden w-[248px] shrink-0 flex-col gap-2 border-r border-hairline p-3 sm:flex"
      style="background: linear-gradient(160deg, var(--color-wash-1), var(--color-wash-2) 55%, var(--color-wash-3));"
    >
      <button class="pill px-3 py-1.5 text-left text-[13px] font-medium" onclick={newSession}>
        + New session
      </button>

      <input
        class="pill mt-1 px-2.5 py-1.5 text-[12.5px]"
        placeholder="Search messages"
        bind:value={query}
        onkeydown={(e) => e.key === 'Enter' && runSearch()}
      />
      {#if searchErr}<div class="text-[11px] text-[#8b3d3d]">{searchErr}</div>{/if}

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
          <button
            class="sesshit {hostState?.session_id === s.id ? 'active' : ''}"
            onclick={() => openSession(s.id)}
          >
            <span class="block truncate text-[12.5px]">{s.title}</span>
            <span class="block text-[11px] text-ink-3">{s.message_count} msg</span>
          </button>
        {/each}
      {/if}
    </aside>

    <!-- main -->
    <main class="flex min-w-0 flex-1 flex-col">
      <div class="scroller min-h-0 flex-1 overflow-y-auto px-6 py-6">
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
            <div class="mt-3 flex gap-2">
              <button class="btn-go" onclick={() => answer('allow')}>Allow</button>
              <button class="btn-ghost" onclick={() => answer('allowSession')}>Allow for session</button>
              <button class="btn-bad" onclick={() => answer('deny')}>Deny</button>
            </div>
          </div>
        </div>
      {/if}

      <!-- composer -->
      <div class="border-t border-hairline px-6 py-3">
        <div class="mx-auto w-full max-w-[780px]">
          <textarea
            class="composer"
            rows="3"
            placeholder="Ask something. Enter to send, Shift+Enter for a newline."
            bind:value={draft}
            onkeydown={onKey}
            disabled={busy}
          ></textarea>

          <div class="mt-2 flex items-center gap-2 text-[12px] text-ink-2">
            <button
              class="btn-go {busy ? 'opacity-40' : ''}"
              onclick={send}
              disabled={busy || !draft.trim()}
            >
              {busy ? 'working…' : 'Send'}
            </button>
            {#if busy}
              <button class="btn-ghost" onclick={() => invoke('cancel_turn')}>Cancel</button>
            {/if}

            <label class="ml-auto flex items-center gap-1">
              approvals
              <select class="pill px-2 py-1 text-[12px]" bind:value={mode} onchange={(e) => setMode((e.target as HTMLSelectElement).value)}>
                <option value="ask">ask</option>
                <option value="session">auto for session</option>
                <option value="auto">full auto</option>
              </select>
            </label>
            <label class="flex items-center gap-1">
              thinking
              <select class="pill px-2 py-1 text-[12px]" bind:value={thinking} onchange={(e) => setThinking((e.target as HTMLSelectElement).value)}>
                <option value="off">off</option>
                <option value="low">low</option>
                <option value="medium">medium</option>
                <option value="high">high</option>
              </select>
            </label>
          </div>
        </div>
      </div>
    </main>
  </div>

  <!-- ========================== telemetry bar ========================= -->
  <footer class="flex h-9 shrink-0 items-center gap-4 border-t border-hairline bg-paper-2 px-4 text-[11.5px] text-ink-2">
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
    <span>spec <b class="text-ink">{fmt.pct(specPct)}</b></span>
    <span>ttft <b class="text-ink">{ttft}</b>ms</span>
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
      <span title="frames sampled during the last turn">
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
