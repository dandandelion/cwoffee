<script lang="ts">
  /**
   * A labelled control with its helper text directly under it.
   *
   * One component because the alternative is a hundred one-off label/hint
   * pairings that drift: the label sizes, the gap, and the difference between
   * "optional, and here is what happens if you leave it" and "this did not
   * work" all come from here.
   */
  import type { Snippet } from 'svelte'

  let {
    label,
    forId,
    hint,
    error,
    wide,
    children,
  }: {
    label: string
    /** Set this when the control inside is a real input. Without it the label is
        still visible but not announced, so a screen reader hears a field with no
        name. A custom control (the Select) passes no forId and carries its own
        aria-label instead. */
    forId?: string
    hint?: string | Snippet
    error?: string
    wide?: boolean
    children: Snippet
  } = $props()

  const hintIsSnippet = $derived(typeof hint === 'function')
</script>

<div class="sfld {wide ? 'wide' : ''}" data-field={label}>
  {#if forId}
    <label class="sfld-l" for={forId}>{label}</label>
  {:else}
    <span class="sfld-l">{label}</span>
  {/if}
  <div class="sfld-c">
    {@render children()}
  </div>
  {#if error}
    <span class="sfld-e" data-field-error>{error}</span>
  {:else if hintIsSnippet}
    <span class="sfld-h">
      {@render (hint as Snippet)()}
    </span>
  {:else if hint}
    <span class="sfld-h">{hint}</span>
  {/if}
</div>
