/**
 * Page-side logging that works in dev AND release.
 *
 * The WebKit console is not reachable from the shell, and "blank window, no
 * error" is the worst failure mode a webview app has: process liveness, memory
 * and sockets all report healthy while nothing painted. So the page must be able
 * to talk to the host from the very first line of JS.
 *
 * In dev this goes to a vite middleware so page and Rust output interleave in
 * one log. In release it goes to the `boot_log` Tauri command. The release path
 * is what caught a real bug: the built bundle ran fine in a browser but never
 * executed inside the release webview, and dev-only logging made that invisible.
 */
let tauriUsable: boolean | null = null

export function pageLog(msg: string) {
  try {
    if (import.meta.env.DEV) {
      void fetch('/__cwoffee_log', {
        method: 'POST',
        body: msg,
        headers: { 'content-type': 'text/plain' },
      }).catch(() => {})
      return
    }
  } catch {
    /* fall through to the tauri path */
  }

  if (tauriUsable === false) return
  try {
    // Imported lazily so a page loaded outside Tauri (a plain browser, a test
    // harness) does not fail at module-evaluation time.
    void import('@tauri-apps/api/core')
      .then(({ invoke }) => invoke('boot_log', { msg }))
      .catch(() => {
        tauriUsable = false
      })
  } catch {
    tauriUsable = false
  }
}
