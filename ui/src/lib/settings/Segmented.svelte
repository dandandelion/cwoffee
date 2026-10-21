<script lang="ts">
  /**
   * A small set of one-word options where seeing all of them at once is the
   * point (an approval mode, a reasoning level, a scope). The control is a
   * radiogroup: the value that is on is drawn on, and the whole set is one tab
   * stop with arrow-key movement, the way a radio group behaves.
   */
  type Opt = { value: string; label: string; title?: string; disabled?: boolean }

  let {
    value,
    options,
    onpick,
    aria,
    disabled = false,
  }: {
    value: string
    options: Opt[]
    onpick: (v: string) => void
    aria: string
    disabled?: boolean
  } = $props()

  function key(e: KeyboardEvent) {
    if (e.key !== 'ArrowRight' && e.key !== 'ArrowLeft') return
    e.preventDefault()
    const step = e.key === 'ArrowRight' ? 1 : -1
    const i = Math.max(
      0,
      options.findIndex((o) => o.value === value),
    )
    const next = options[(i + step + options.length) % options.length]
    if (next && !next.disabled) onpick(next.value)
  }
</script>

<div class="sseg" role="radiogroup" tabindex="-1" aria-label={aria} onkeydown={key}>
  {#each options as o (o.value)}
    <button
      class="sseg-b {value === o.value ? 'on' : ''}"
      type="button"
      role="radio"
      aria-checked={value === o.value ? 'true' : 'false'}
      data-seg-value={o.value}
      title={o.title}
      disabled={disabled || o.disabled}
      tabindex={value === o.value || !options.some((x) => x.value === value) ? 0 : -1}
      onclick={() => onpick(o.value)}
    >
      {o.label}
    </button>
  {/each}
</div>
