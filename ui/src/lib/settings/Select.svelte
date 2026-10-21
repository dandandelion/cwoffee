<script lang="ts">
  /**
   * A select the app draws itself, in the app's colours, in both themes.
   *
   * This is not decoration. A native <select> in a webview renders the toolkit's
   * own menu: a bright white panel with the system font on a dark screen, and on
   * some platforms the closed control is styled but the open one is not, which
   * means the part people actually look at is the part the design does not reach.
   * The rest of this app already avoids native selects for that reason (see the
   * popovers in App.svelte); this is the same rule applied to a form control.
   *
   * The panel is positioned inside the scroll container it lives in and flipped
   * when the room below is smaller than the panel, rather than fixed to the
   * viewport: the settings panel is itself transformed and scrolled, and a fixed
   * descendant of a transformed ancestor is clipped by it.
   */
  import { tick } from 'svelte'

  type Opt = { value: string; label: string; hint?: string; disabled?: boolean }

  let {
    value,
    options,
    onpick,
    aria,
    id,
    disabled = false,
    mono = false,
    placeholder = 'choose',
  }: {
    value: string
    options: Opt[]
    onpick: (v: string) => void
    /** The control announces itself; a visible label next to it is not a <label for>. */
    aria: string
    id?: string
    disabled?: boolean
    mono?: boolean
    placeholder?: string
  } = $props()

  let open = $state(false)
  let up = $state(false)
  let cursor = $state(0)
  let wrap = $state<HTMLDivElement | null>(null)
  let btn = $state<HTMLButtonElement | null>(null)
  let panel = $state<HTMLDivElement | null>(null)
  const panelId = $derived(`ssel_list_${id ?? 'x'}`)
  let panelMax = $state(232)

  const current = $derived(options.find((o) => o.value === value) ?? null)

  function room() {
    if (!wrap || !btn) return
    const r = btn.getBoundingClientRect()
    // The nearest scrollable ancestor is the settings column; the panel belongs to it.
    let box: HTMLElement | null = wrap.parentElement
    while (box && !/(auto|scroll)/.test(getComputedStyle(box).overflowY)) box = box.parentElement
    const lim = box ? box.getBoundingClientRect() : { top: 0, bottom: innerHeight }
    const below = lim.bottom - r.bottom - 8
    const above = r.top - lim.top - 8
    const want = Math.min(232, 26 + options.length * 34)
    if (below >= want || below >= above) {
      up = false
      panelMax = Math.max(90, Math.min(232, below - 4))
    } else {
      up = true
      panelMax = Math.max(90, Math.min(232, above - 4))
    }
  }

  async function show(next: boolean) {
    if (next === open) return
    open = next
    if (!open) return
    cursor = Math.max(
      0,
      options.findIndex((o) => o.value === value),
    )
    if (cursor < 0) cursor = 0
    room()
    await tick()
    panel?.querySelector<HTMLElement>(`[data-idx="${cursor}"]`)?.scrollIntoView({ block: 'nearest' })
    btn?.focus()
  }

  function pickAt(i: number) {
    const o = options[i]
    if (!o || o.disabled) return
    show(false)
    onpick(o.value)
  }

  function onkey(e: KeyboardEvent) {
    if (e.key === 'Escape' && open) {
      // Esc unwinds the open menu, not the whole settings screen behind it.
      e.stopPropagation()
      e.preventDefault()
      void show(false)
      return
    }
    if (!open && (e.key === 'ArrowDown' || e.key === 'ArrowUp' || e.key === 'Enter' || e.key === ' ')) {
      e.preventDefault()
      void show(true)
      return
    }
    if (!open) return
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault()
      const step = e.key === 'ArrowDown' ? 1 : -1
      let i = cursor
      for (let n = 0; n < options.length; n++) {
        i = (i + step + options.length) % options.length
        if (!options[i]?.disabled) break
      }
      cursor = i
      void tick().then(() =>
        panel?.querySelector<HTMLElement>(`[data-idx="${cursor}"]`)?.scrollIntoView({ block: 'nearest' }),
      )
      return
    }
    if (e.key === 'Home' || e.key === 'End') {
      e.preventDefault()
      cursor = e.key === 'Home' ? 0 : options.length - 1
      return
    }
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault()
      pickAt(cursor)
      return
    }
    if (e.key === 'Tab') {
      void show(false)
      return
    }
    if (e.key.length === 1 && /\S/.test(e.key)) {
      const want = e.key.toLowerCase()
      for (let n = 1; n <= options.length; n++) {
        const i = (cursor + n) % options.length
        if ((options[i]?.label ?? '').toLowerCase().startsWith(want)) {
          e.preventDefault()
          cursor = i
          break
        }
      }
    }
  }

  function away(e: PointerEvent) {
    if (!open) return
    if (wrap?.contains(e.target as Node)) return
    void show(false)
  }
</script>

<svelte:window onpointerdown={away} />

<div class="ssel {open ? 'open' : ''} {up ? 'up' : ''} {mono ? 'mono' : ''}" bind:this={wrap}>
  <button
    class="ssel-btn"
    type="button"
    role="combobox"
    id={id}
    bind:this={btn}
    disabled={disabled}
    aria-autocomplete="none"
    aria-controls={panelId}
    aria-haspopup="listbox"
    aria-expanded={open ? 'true' : 'false'}
    aria-label={aria}
    aria-activedescendant={open ? `ssel-opt_${id ?? 'x'}_${cursor}` : undefined}
    onclick={() => void show(!open)}
    onkeydown={onkey}
  >
    <span class="ssel-v {current ? '' : 'ph'}">{current?.label ?? placeholder}</span>
    <svg class="ssel-a" width="9" height="9" viewBox="0 0 10 10" aria-hidden="true">
      <path d={open ? 'M2 6.5L5 3l3 3.5' : 'M2 3.5L5 7l3-3.5'} stroke="currentColor" stroke-width="1.4" fill="none" stroke-linecap="round" stroke-linejoin="round"/>
    </svg>
  </button>

  {#if open}
    <div
      class="ssel-pop"
      role="listbox"
      id={panelId}
      bind:this={panel}
      aria-label={aria}
      style="max-height:{panelMax}px"
    >
      {#each options as o, i (o.value)}
        <button
          class="ssel-opt {value === o.value ? 'sel' : ''} {cursor === i ? 'cur' : ''} {o.disabled ? 'dis' : ''}"
          type="button"
          role="option"
          tabindex="-1"
          id="ssel-opt_{id ?? 'x'}_{i}"
          data-sel-value={o.value}
          data-idx={i}
          aria-selected={value === o.value ? 'true' : 'false'}
          onpointerdown={(e: PointerEvent) => {
            // pointerdown on the panel would otherwise be treated as "away"; stop it here
            e.stopPropagation()
          }}
          onclick={() => pickAt(i)}
          onpointerenter={() => (cursor = i)}
        >
          <span class="ssel-ck" aria-hidden="true">{value === o.value ? '\u2713' : ''}</span>
          <span class="ssel-t">
            <span class="ssel-l">{o.label}</span>
            {#if o.hint}<span class="ssel-h">{o.hint}</span>{/if}
          </span>
        </button>
      {/each}
    </div>
  {/if}
</div>
