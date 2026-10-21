<script lang="ts">
  /**
   * One section of a settings page: a heading, one sentence saying what the
   * controls below it change, and the controls.
   *
   * The description lives here rather than in the navigation rail on purpose.
   * A rail sentence is a caption for a label nobody is reading yet; the same
   * sentence next to the thing it describes is read at the moment it matters.
   */
  import type { Snippet } from 'svelte'

  let {
    id,
    title,
    desc,
    actions,
    children,
  }: {
    /** Stable hook: deep links scroll to it and the app's own harness reads it. */
    id: string
    title: string
    desc?: string
    /** Right-aligned controls that act on the whole section, e.g. "expand all". */
    actions?: Snippet
    children: Snippet
  } = $props()
</script>

<section class="ssect" data-set-section={id} id={`set-${id}`} tabindex="-1">
  <header class="ssect-head">
    <div class="ssect-hh">
      <h3 class="ssect-h">{title}</h3>
      {#if desc}
        <p class="ssect-d">{desc}</p>
      {/if}
    </div>
    {#if actions}
      <div class="ssect-acts">
        {@render actions()}
      </div>
    {/if}
  </header>
  <div class="ssect-b">
    {@render children()}
  </div>
</section>
