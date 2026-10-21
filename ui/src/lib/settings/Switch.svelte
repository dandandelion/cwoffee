<script lang="ts">
  /** A real on/off control: `role="switch"`, announced, keyboard-operable.
      The gateway switch in the old popup was a row that happened to contain a
      drawn toggle, which meant the row was the button and the toggle was decoration. */
  let {
    on,
    onpick,
    aria,
    disabled = false,
  }: {
    on: boolean
    onpick: (next: boolean) => void
    aria: string
    disabled?: boolean
  } = $props()
</script>

<button
  class="ssw {on ? 'on' : ''}"
  type="button"
  role="switch"
  aria-checked={on ? 'true' : 'false'}
  aria-label={aria}
  data-switch={aria}
  disabled={disabled}
  onclick={() => onpick(!on)}
>
  <i aria-hidden="true"></i>
</button>
