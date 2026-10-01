<script lang="ts">
  import { onMount, onDestroy } from 'svelte'
  import { invoke, Channel } from '@tauri-apps/api/core'
  import { getCurrentWindow } from '@tauri-apps/api/window'
  import type { EngineEvent, GpuReport } from './lib/events'
  import { LiveWindow, Rolling, statsFromTurn, fmt } from './lib/live'
  import { pageLog } from './lib/pagelog'

  // ---- state -------------------------------------------------------------
  let streamEl: HTMLPreElement | undefined = $state()
  let streaming = $state(false)
  let model = $state('')
  let tokPerSec = $state(50)
  let words = $state(140)
  let gpu = $state<GpuReport | null>(null)
  // IPC health is surfaced in the footer. An empty catch here once hid a CSP
  // misconfiguration that killed every invoke, and every other gate still passed.
  let ipc = $state<{ ok: boolean; msg: string }>({ ok: false, msg: 'not tried' })
  let lastUsage = $state<Record<string, number> | null>(null)
  let turnCount = $state(0)

  // ---- telemetry ---------------------------------------------------------
  const live = new LiveWindow(1000)
  const rolling = new Rolling(10)

  let curDecode = $state(0)
  let avgDecode = $state(0)
  let curPrefill = $state(0) // effective, from TTFT
  let avgPrefill = $state(0) // raw, from server
  let cacheHit = $state(0)
  let cachedTokens = $state(0)
  let promptTokens = $state(0)
  let specPct = $state(0)

  // ---- jank meter: the M0 acceptance criterion is "no jank" --------------
  // We sample rAF spacing while streaming. A dropped frame is >33ms.
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

  // ---- streaming hot path -------------------------------------------------
  // PLAN.md §9 rule 3: deltas are appended as text nodes and batched on rAF.
  // We never re-render the message and never parse markdown mid-stream.
  let pending: string[] = []
  let flushRaf = 0

  function flush() {
    flushRaf = 0
    if (!pending.length || !streamEl) return
    // one append for everything that arrived in this frame
    streamEl.appendChild(document.createTextNode(pending.join('')))
    pending = []
    const now = performance.now()
    live.mark(now)
    curDecode = live.decodeTps()
  }

  function pushText(s: string) {
    pending.push(s)
    if (!flushRaf) flushRaf = requestAnimationFrame(flush)
  }

  // ---- event handling -----------------------------------------------------
  function handle(ev: EngineEvent) {
    switch (ev.kind) {
      case 'turnStart':
        model = ev.d.model
        streaming = true
        turnCount++
        live.reset()
        frames = 0
        dropped = 0
        worstFrame = 0
        lastFrameTs = 0
        rafId = requestAnimationFrame(watchFrames)
        if (streamEl) streamEl.textContent = ''
        pageLog('turn: start')
        break
      case 'text':
        pushText(ev.d)
        break
      case 'reasoning':
        // M0: reasoning is shown dimmed and prefixed, not parsed
        pushText('')
        break
      case 'toolCallBegin':
        pushText(`\n\u25b8 ${ev.d.name}(`)
        break
      case 'toolCallArgs':
        pushText(ev.d)
        break
      case 'toolCallReady':
        pushText(')\n')
        break
      case 'toolExecOutput':
        pushText(ev.d.chunk)
        break
      case 'toolExecEnd':
        pushText(`${ev.d.ok ? '\u2713' : '\u2717'} exit ${ev.d.exit_code ?? '?'}\n\n`)
        break
      case 'usage': {
        lastUsage = ev.d as unknown as Record<string, number>
        const s = statsFromTurn(ev.d)
        rolling.push(s)
        curPrefill = s.effectivePrefillTps
        avgPrefill = rolling.avgRawPrefillTps()
        avgDecode = rolling.avgDecodeTps()
        cacheHit = s.cacheHitPct
        cachedTokens = ev.d.cached_tokens
        promptTokens = ev.d.prompt_tokens
        specPct = s.specAcceptPct
        break
      }
      case 'turnEnd':
        // final flush so nothing is stranded in the queue
        if (flushRaf) cancelAnimationFrame(flushRaf)
        flushRaf = 0
        flush()
        streaming = false
        if (rafId) cancelAnimationFrame(rafId)
        rafId = 0
        pageLog(
          `turn: end chars=${streamEl?.textContent.length ?? 0} ` +
            `frames=${frames} dropped=${dropped} worst=${worstFrame.toFixed(0)}ms ` +
            `decode=${curDecode.toFixed(1)}/s`,
        )
        break
      case 'error':
        pushText(`\n! ${ev.d.message}\n`)
        if (ev.d.fatal) streaming = false
        break
    }
  }

  async function runTurn() {
    if (streaming) return
    const onEvent = new Channel<EngineEvent>()
    onEvent.onmessage = handle
    try {
      await invoke('start_fake_turn', { onEvent, tokPerSec, words })
    } catch (e) {
      pushText(`\n! invoke failed: ${e}\n`)
      streaming = false
    }
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
    // are snake_case on the wire. Getting this wrong is a silent-ish
    // "missing field `stream_el`" invalid-args error.
    const report = {
      titlebar: !!document.querySelector('.titlebar'),
      sidebar: !!document.querySelector('aside'),
      footer: !!document.querySelector('footer'),
      stream_el: !!document.querySelector('pre.stream'),
      body_bg: getComputedStyle(document.body).backgroundColor,
      overlay_present: !!document.querySelector('vite-error-overlay'),
      text_len: document.body.innerText.trim().length,
      device_pixel_ratio: window.devicePixelRatio,
    }
    try {
      await invoke('render_report', { report })
      ipc = { ok: true, msg: 'ok' }
    } catch (e) {
      ipc = { ok: false, msg: String(e).slice(0, 120) }
      pageLog(`render_report invoke FAILED: ${String(e)} | internals=${typeof (window as any).__TAURI_INTERNALS__}`)
    }
  }

  onMount(async () => {
    try {
      gpu = await invoke<GpuReport>('gpu_report')
      ipc = { ok: true, msg: 'ok' }
    } catch (e) {
      gpu = null
      ipc = { ok: false, msg: String(e).slice(0, 120) }
      pageLog(`gpu_report invoke FAILED: ${String(e)} | internals=${typeof (window as any).__TAURI_INTERNALS__}`)
    }
    if (winErr) pageLog(`getCurrentWindow FAILED: ${winErr}`)
    pageLog(`mounted: titlebar=${!!document.querySelector('.titlebar')} text=${document.body.innerText.trim().length}`)
    // let fonts + Tailwind settle before reporting
    setTimeout(() => void reportRender('mount'), 400)
    setTimeout(() => void reportRender('late'), 2000)

    // Dev harness: `VITE_AUTOTURN=1 ./scripts/dev.sh` (or ?autoturn in the URL)
    // streams a turn on launch. Proves the Channel -> rAF -> text-node path
    // without anyone having to click.
    if (location.search.includes('autoturn') || import.meta.env.VITE_AUTOTURN) {
      setTimeout(() => void runTurn(), 1200)
    }
  })

  onDestroy(() => {
    if (rafId) cancelAnimationFrame(rafId)
    if (flushRaf) cancelAnimationFrame(flushRaf)
  })

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
    <span class="ml-2 text-[11px] text-ink-3">M0 skeleton</span>

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
    <!-- sidebar: just the wash, to prove the gradient renders -->
    <aside
      class="hidden w-[248px] shrink-0 flex-col gap-1 border-r border-hairline p-3 sm:flex"
      style="background: linear-gradient(160deg, var(--color-wash-1), var(--color-wash-2) 55%, var(--color-wash-3));"
    >
      <div class="pill px-3 py-1.5 text-[13px] font-medium">General</div>
      <div class="mt-3 px-1 text-[11px] uppercase tracking-wide text-ink-3">M0</div>
      <div class="px-1 py-1 text-[13px] text-ink-2">New Task</div>
      <div class="px-1 py-1 text-[13px] text-ink-2">Search</div>
    </aside>

    <!-- main -->
    <main class="flex min-w-0 flex-1 flex-col">
      <div class="flex min-h-0 flex-1 flex-col items-center overflow-y-auto px-6 py-8">
        <div class="w-full max-w-[760px]">
          <h1 class="text-[26px] font-semibold tracking-tight">Streaming proof</h1>
          <p class="mt-1 text-[14px] text-ink-3">
            {model || 'no turn yet'} — plain-text append, rAF-batched, no markdown until stream end
          </p>

          <div class="card mt-5 min-h-[300px] p-4">
            <pre class="stream" bind:this={streamEl}></pre>
            {#if streaming}<span class="caret"></span>{/if}
          </div>

          <!-- composer stand-in -->
          <div class="card mt-4 flex items-center gap-3 px-3 py-2.5">
            <button
              class="rounded-[10px] bg-go px-3.5 py-1.5 text-[13px] font-medium text-paper disabled:opacity-40"
              onclick={runTurn}
              disabled={streaming}
            >
              {streaming ? 'streaming…' : 'Run M0 turn'}
            </button>

            <label class="flex items-center gap-1.5 text-[12px] text-ink-2">
              tok/s
              <input
                class="pill w-16 px-2 py-1 text-[12px]"
                type="number"
                bind:value={tokPerSec}
                min="1"
                max="5000"
              />
            </label>
            <label class="flex items-center gap-1.5 text-[12px] text-ink-2">
              words
              <input
                class="pill w-16 px-2 py-1 text-[12px]"
                type="number"
                bind:value={words}
                min="1"
                max="4000"
              />
            </label>

            <span class="ml-auto text-[12px] text-ink-3">turns: {turnCount}</span>
          </div>
        </div>
      </div>
    </main>
  </div>

  <!-- ========================== telemetry bar ========================= -->
  <footer class="flex h-9 shrink-0 items-center gap-4 border-t border-hairline bg-paper-2 px-4 text-[11.5px] text-ink-2">
    <span title="prefix cache hit">
      ⚡ cache <b class="text-ink">{fmt.pct(cacheHit)}</b>
      <span class="text-ink-3">({cachedTokens}/{promptTokens})</span>
    </span>
    <span title="effective = prompt_tokens / ttft">
      prefill <b class="text-ink">{fmt.rate(curPrefill)}</b>
      <span class="text-ink-3">eff · avg {fmt.rate(avgPrefill)} raw</span>
    </span>
    <span title="live arrival rate vs session average">
      decode <b class="text-ink">{fmt.rate(curDecode)}</b>
      <span class="text-ink-3">· avg {fmt.rate(avgDecode)}</span>
    </span>
    <span>spec <b class="text-ink">{fmt.pct(specPct)}</b></span>

    <span class="ml-auto flex items-center gap-3">
      <span
        class="rounded-full px-2 py-0.5 {ipc.ok ? 'bg-[#eef0e8] text-ink-3' : 'bg-[#f6e6e6] text-[#8b3d3d]'}"
        title="Tauri IPC round-trip"
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
