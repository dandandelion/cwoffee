/**
 * Random pauses for the animated pictures.
 *
 * An animated image in an `<img>` cannot be paused, slowed or restarted from JS: the animation
 * lives inside the file's frame table, not in the document. WebKit honours a duration written
 * into the container and ignores every DOM or CSS attempt to touch it — which is why
 * `prefers-reduced-motion` gets a *different file* for these pictures rather than a style.
 * It also means the only way to change the gap at the end of a loop is to edit those bytes, so
 * that is what this does, in memory, starting from the artist's exact bitstream: walk the RIFF
 * chunks, rewrite the last frame's duration, hand the `<img>` a `blob:` URL for the patched
 * copy. No re-encode, no second generation of quality, no extra bytes on disk.
 *
 * Because a cycle costs three bytes and one object URL, the gap can be *random*: each cycle
 * re-arms the picture with a fresh hold drawn from [minMs, maxMs] and schedules the next swap
 * for the moment that hold ends. A baked-in fixed hold (the opening-screen cup ships with one)
 * ticks like a metronome; the drawing is meant to read as alive.
 *
 * **The byte offset is calibrated, not derived from the spec.** Duration is read from 12 bytes
 * into an `ANMF` chunk's payload; that was established against `scripts/webp-pause.py` (whose
 * edits `identify` and this engine both report and honour) rather than from a document. Every
 * chunk is checked for the image fourcc that has to follow the frame header, and a layout this
 * code does not recognise returns `null` — the caller then leaves the picture exactly as it
 * shipped instead of writing over someone else's container.
 */

import { pageLog } from './pagelog'

/** Frame table of a loop this code understands. */
export type AniFrames = {
  /** Sum of every frame's duration: one full lap of the loop, holds included. */
  playMs: number
  /** Where the last frame's duration lives in the buffer. */
  holdAt: number
  /** That last frame's duration as shipped. */
  lastMs: number
  /** How many frames the loop has. */
  frames: number
}

const CHUNK_WALK_LIMIT = 4096

function fourcc(b: Uint8Array, at: number): string {
  // Bounds-checked and reading four *consecutive* bytes: reading the same byte four times makes
  // every chunk look like 'AAAA', which is how this code once reported a valid animation as
  // unreadable - the page check caught it because the cup it armed was never armed at all.
  if (at < 0 || at + 4 > b.length) return ''
  return String.fromCharCode(b[at], b[at + 1], b[at + 2], b[at + 3])
}

function read24(b: Uint8Array, at: number): number {
  return b[at] | (b[at + 1] << 8) | (b[at + 2] << 16)
}

function write24(b: Uint8Array, at: number, v: number) {
  b[at] = v & 0xff
  b[at + 1] = (v >> 8) & 0xff
  b[at + 2] = (v >> 16) & 0xff
}

/**
 * Locate the frame table. Returns null for anything that is not an animation this engine's
 * encoder produced (a still, a different container, an unexpected ANMF layout).
 */
export function aniFrames(b: Uint8Array): AniFrames | null {
  if (b.length < 20 || fourcc(b, 0) !== 'RIFF' || fourcc(b, 8) !== 'WEBP') return null
  let i = 12
  let playMs = 0
  let holdAt = -1
  let lastMs = 0
  let frames = 0
  while (i + 8 <= b.length && frames < CHUNK_WALK_LIMIT) {
    const size = b[i + 4] | (b[i + 5] << 8) | (b[i + 6] << 16) | (b[i + 7] << 24)
    if (size < 0 || i + 8 + size > b.length) return null // truncated or lying about its length
    if (fourcc(b, i) === 'ANMF') {
      const p = i + 8
      // Frame header is 16 bytes, and the frame's own image chunk has to start right after it.
      // Checking that is what distinguishes "our layout" from "some other layout".
      const inner = fourcc(b, p + 16)
      if (inner !== 'ALPH' && inner !== 'VP8 ' && inner !== 'VP8L') return null
      playMs += read24(b, p + 12)
      holdAt = p + 12
      lastMs = read24(b, p + 12)
      frames++
    }
    i += 8 + size + (size & 1) // chunks are word-padded
  }
  if (frames < 2 || holdAt < 0) return null
  return { playMs, holdAt, lastMs, frames }
}

/** The same bytes with the loop's final frame held for `holdMs` instead of whatever it said. */
export function withHold(b: Uint8Array, frames: AniFrames, holdMs: number): Uint8Array<ArrayBuffer> {
  const out = new Uint8Array(new ArrayBuffer(b.length)) // a copy over a plain ArrayBuffer, which is
  // what a Blob wants; the cached original stays untouched.
  out.set(b)
  write24(out, frames.holdAt, Math.max(1, Math.min(0xffffff, Math.round(holdMs))))
  return out
}

/** One fetch per URL for the whole session; the picture is large and every copy shares it. */
const bytesByUrl = new Map<string, Promise<Uint8Array | null>>()

/**
 * Get the container bytes. `fetch` first, then `XMLHttpRequest`: on some builds of this app's own
 * asset origin `fetch` refuses same-origin requests that an `<img>` for the very same URL handles
 * fine, and a picture that only ever loads as an image would otherwise never be patchable. The
 * reason goes to the page log, because "the pause did not apply" is unfindable without it.
 */
async function readBytes(url: string): Promise<Uint8Array | null> {
  try {
    const r = await fetch(url)
    if (r.ok) return new Uint8Array(await r.arrayBuffer())
    pageLog(`anipause: fetch ${url} answered HTTP ${r.status}, trying XHR`)
  } catch (e) {
    pageLog(`anipause: fetch ${url} failed (${String(e)}), trying XHR`)
  }
  return await new Promise<Uint8Array | null>((res) => {
    const x = new XMLHttpRequest()
    x.open('GET', url)
    x.responseType = 'arraybuffer'
    x.onload = () => {
      if (x.status >= 200 && x.status < 400 && x.response) res(new Uint8Array(x.response as ArrayBuffer))
      else {
        pageLog(`anipause: XHR ${url} answered ${x.status}`)
        res(null)
      }
    }
    x.onerror = () => {
      pageLog(`anipause: XHR ${url} failed`)
      res(null)
    }
    x.send()
  })
}

export function aniBytes(url: string): Promise<Uint8Array | null> {
  let p = bytesByUrl.get(url)
  if (!p) {
    p = readBytes(url).then((b) => {
      if (!b) return null
      const fr = aniFrames(b)
      if (!fr) {
        pageLog(`anipause: ${url} is not an animation this code can read; leaving it as authored`)
        return null
      }
      pageLog(`anipause: ${url} armed for random holds (${fr.playMs}ms lap, ${fr.frames} frames, last frame ${fr.lastMs}ms)`)
      return b
    })
    bytesByUrl.set(url, p)
  }
  return p
}

/**
 * Swap a little before the patched loop would restart on its own, so the container's own
 * wrap-around is never visible: the picture is held on its last frame until the new copy takes
 * over, and the last frame is the resting pose the loop starts from, so the handover shows no
 * jump. A swap that lands late has already shown a few milliseconds of the next lap.
 */
const SWAP_EARLY_MS = 40

/** A hidden webview desynchronises timers from paint; re-arm from frame 0 when it comes back. */

/**
 * Keeps one `<img>` looping with a random hold at the end of every lap.
 *
 * `stop()` ends the chain and releases the object URL. The chain also ends by itself if the
 * element leaves the document — the transcript throws its DOM away on every reload of a
 * session, and a timer that outlives its element is a leak that repaints nothing.
 */
/**
 * Which shipped asset an armed element is currently playing. The `<img>` only knows the blob URL,
 * and the render report is the one place an installed build says what it loaded - without this it
 * would name a random UUID instead of the picture.
 */
const armedFrom = new WeakMap<HTMLImageElement, string>()

export function armedFromUrl(img: HTMLImageElement): string | null {
  return armedFrom.get(img) ?? null
}

export class LoopHold {
  private timer: number | null = null
  private objUrl: string | null = null
  private stopped = false
  private readonly onVis = () => {
    if (!this.stopped && document.visibilityState === 'visible') this.arm()
  }

  constructor(
    private readonly img: HTMLImageElement,
    private readonly url: string,
    private readonly minMs: number,
    private readonly maxMs: number,
  ) {
    document.addEventListener('visibilitychange', this.onVis)
    void this.start()
  }

  private async start() {
    const bytes = await aniBytes(this.url)
    if (this.stopped || !bytes) return // fetch failed or the picture is not ours to patch:
    // leave whatever `src` the caller set (the shipped file) and let it loop as authored.
    this.frames = aniFrames(bytes)
    this.bytes = bytes
    this.arm()
  }

  private frames: AniFrames | null = null
  private bytes: Uint8Array | null = null

  private arm() {
    if (this.stopped || !this.bytes || !this.frames) return
    if (!this.img.isConnected) return this.stop()
    const hold = this.minMs + Math.random() * (this.maxMs - this.minMs)
    const patched = withHold(this.bytes, this.frames, hold)
    const next = URL.createObjectURL(new Blob([patched], { type: 'image/webp' }))
    armedFrom.set(this.img, this.url)
    const prev = this.objUrl
    this.objUrl = next
    this.img.src = next
    if (prev) URL.revokeObjectURL(prev)
    // The lap runs to (everything before the last frame) + this hold; arm the next copy for
    // just before that, and recompute the hold each time so consecutive laps differ.
    const lapMs = this.frames.playMs - this.frames.lastMs + hold
    this.timer = window.setTimeout(() => this.arm(), Math.max(0, lapMs - SWAP_EARLY_MS))
  }

  stop() {
    this.stopped = true
    if (this.timer !== null) window.clearTimeout(this.timer)
    this.timer = null
    if (this.objUrl) URL.revokeObjectURL(this.objUrl)
    this.objUrl = null
    document.removeEventListener('visibilitychange', this.onVis)
  }
}

/** Reduced motion is a different file, decided by whoever picks the picture. */
export function prefersStill(): boolean {
  return matchMedia('(prefers-reduced-motion: reduce)').matches
}

/**
 * The pause every resting cup draws its holds from: 4s is long enough to notice the picture has
 * stopped, 8s short enough to see it move twice in a sitting. Shared by the cup under a turn's
 * name and the cup on the opening screen, because two different resting intervals in one app
 * would read as two different moods. Random per lap, so neither ticks to a metronome.
 */
export const IDLE_HOLD: [number, number] = [4000, 8000]
