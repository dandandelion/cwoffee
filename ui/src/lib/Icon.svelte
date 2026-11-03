<script lang="ts">
  // One stroke-drawn icon set for the window, drawn in the app's own hand: 16px grid, currentColor,
  // round caps, no fills. A component rather than a folder of files, because this is twenty paths
  // and each is written once — and because an icon copy-pasted as raw `<svg>` in three places is how
  // one button ends up a different weight than the neighbour beside it.
  //
  // `label` is what lets an icon *be* the control instead of a decoration under it: the accessible
  // name comes from the same place the picture comes from, so a keyboard and a screen reader reach
  // the same button a mouse does. `decorative` marks the ones that sit beside their own word.

  const PATHS = {
    browser:
      '<rect x="2.2" y="3.2" width="11.6" height="9.6" rx="1.4"/><path d="M2.2 5.8h11.6"/><circle cx="4.1" cy="4.5" r=".5" fill="currentColor" stroke="none"/><circle cx="5.7" cy="4.5" r=".5" fill="currentColor" stroke="none"/>',
    globe:
      '<circle cx="8" cy="8" r="5.8"/><path d="M2.2 8h11.6M8 2.2c1.7 1.9 1.7 9.8 0 11.6M8 2.2c-1.7 1.9-1.7 9.8 0 11.6"/>',
    lock: '<rect x="4" y="7.2" width="8" height="6.2" rx="1.2"/><path d="M5.8 7.2V5.6a2.2 2.2 0 0 1 4.4 0v1.6"/>',
    x: '<path d="M4.4 4.4l7.2 7.2M11.6 4.4l-7.2 7.2"/>',
    back: '<path d="M10.2 3.8 5.6 8l4.6 4.2"/>',
    forward: '<path d="M5.8 3.8 10.4 8l-4.6 4.2"/>',
    reload: '<path d="M13 8a5 5 0 1 1-1.6-3.7"/><path d="M13.2 2.4v2.8h-2.8"/>',
    power: '<path d="M8 2.8v5"/><path d="M4.9 4.6a4.6 4.6 0 1 0 6.2 0"/>',
    check: '<path d="M3.6 8.4 6.4 11l6-6.4"/>',
    keyboard:
      '<rect x="2.2" y="4.6" width="11.6" height="6.8" rx="1.3"/><path d="M4.7 7h.01M6.7 7h.01M8.7 7h.01M10.7 7h.01M5.5 9.5h5"/>',
    activity: '<path d="M2.2 8h2.3l1.7-4.2 2.4 8.4 1.9-5 1.3 2.8h3.2"/>',
    info: '<circle cx="8" cy="8" r="5.8"/><path d="M8 7.3v3.4M8 5.3h.01"/>',
    page: '<path d="M4 2.6h5.3L12 5.3v8.1H4z"/><path d="M9.2 2.8v2.6h2.6"/>',
    alert: '<path d="M8 2.9 13.4 12H2.6z"/><path d="M8 6.5v2.9M8 11.1h.01"/>',
    download: '<path d="M8 3v6.4M4.9 6.6 8 9.6l3.1-3"/><path d="M3.2 12.4h9.6"/>',
    person: '<circle cx="8" cy="6" r="2.4"/><path d="M3.6 13.2a4.6 4.6 0 0 1 8.8 0"/>',
    model:
      '<path d="M8 2.4 9.3 5.9l3.5 1.3-3.5 1.3L8 12l-1.3-3.5L3.2 7.2l3.5-1.3z"/><path d="M12.7 11.3l.5 1.5 1.5.5-1.5.5-.5 1.5-.5-1.5-1.5-.5 1.5-.5z"/>',
    clock: '<circle cx="8" cy="8" r="5.8"/><path d="M8 4.9V8l2.2 1.6"/>',
    pointer: '<path d="M4 3.2 12 7.4l-3.4 1.2L7.4 12z"/>',
    send: '<path d="M2.8 8h9.4M9 4.8 12.4 8 9 11.2"/>',
    stop: '<rect x="4.6" y="4.6" width="6.8" height="6.8" rx="1"/>',
    plug: '<path d="M5.4 2.6v3.2M10.6 2.6v3.2"/><path d="M4 5.8h8v2.2a4 4 0 0 1-8 0z"/><path d="M8 12v1.6"/>',
  /* Full window and back. Four corner brackets pointing out, then the same four pointing in: the
     pair reads as one control that reverses, which is what the button is. */
  expand:
    '<path d="M2.6 6.2V2.6h3.6M13.4 9.8v3.6H9.8M9.8 2.6h3.6v3.6M6.2 13.4H2.6V9.8"/>',
  shrink:
    '<path d="M6.2 2.6v3.6H2.6M9.8 13.4V9.8h3.6M9.8 2.6v3.6h3.6M6.2 13.4V9.8H2.6"/>',
  } as const

  type IconName = keyof typeof PATHS

  let {
    name,
    size = 16,
    label = '',
    decorative = false,
    stroke = 1.6,
  }: {
    name: IconName
    size?: number
    /** The accessible name. With `decorative`, the icon is a decoration beside its own word. */
    label?: string
    decorative?: boolean
    stroke?: number
  } = $props()

  const body = $derived(PATHS[name])
  const bare = $derived(decorative || !label)
</script>

{#if !bare}
  <span class="ic-sr">{label}</span>
{/if}
<svg
  class="ic"
  width={size}
  height={size}
  viewBox="0 0 16 16"
  aria-hidden={bare ? 'true' : undefined}
  aria-label={bare ? undefined : label}
  role={bare ? undefined : 'img'}
  fill="none"
  stroke="currentColor"
  stroke-width={stroke}
  stroke-linecap="round"
  stroke-linejoin="round"
>
  {@html body}
</svg>

<style>
  .ic {
    display: block;
    flex: none;
  }
  .ic-sr {
    position: absolute;
    width: 1px;
    height: 1px;
    margin: -1px;
    padding: 0;
    overflow: hidden;
    clip-path: inset(50%);
    white-space: nowrap;
    border: 0;
  }
</style>
