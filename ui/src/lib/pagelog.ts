/**
 * Dev-only escape hatch. The WebKit console is not reachable from the shell, and
 * "blank page, no error" is the worst failure mode a webview app can have.
 * The vite dev server exposes POST /__cowry_log which prints into the same log
 * as the Rust side, so page-side and host-side output line up chronologically.
 *
 * Production builds never call this (the endpoint does not exist).
 */
export function pageLog(msg: string) {
  if (import.meta.env.PROD) return
  try {
    void fetch('/__cowry_log', {
      method: 'POST',
      body: msg,
      headers: { 'content-type': 'text/plain' },
    })
  } catch {
    /* logging must never throw */
  }
}
