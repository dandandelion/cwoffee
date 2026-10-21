<script lang="ts">
  /**
   * Radio rows: one option per row, each with the sentence that says what
   * choosing it actually does. Used where the options need more than a word
   * (an approval mode, a search provider), which is where a segmented control
   * would force the explanation into a tooltip nobody opens.
   */
  type Choice = { value: string; label: string; sub?: string; disabled?: boolean }

  let {
    value,
    options,
    onpick,
    aria,
    disabled = false,
  }: {
    value: string
    options: Choice[]
    onpick: (v: string) => void
    aria: string
    disabled?: boolean
  } = $props()
</script>

<div class="schoice-set" role="radiogroup" aria-label={aria}>
  {#each options as o (o.value)}
    <button
      class="schoice {value === o.value ? 'on' : ''}"
      type="button"
      role="radio"
      aria-checked={value === o.value ? 'true' : 'false'}
      data-choice-value={o.value}
      disabled={disabled || o.disabled}
      onclick={() => onpick(o.value)}
    >
      <span class="schoice-dot" aria-hidden="true"></span>
      <span class="schoice-t">
        <span class="schoice-l">{o.label}</span>
        {#if o.sub}<span class="schoice-s">{o.sub}</span>{/if}
      </span>
    </button>
  {/each}
</div>
