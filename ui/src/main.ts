import { mount } from 'svelte'
import App from './App.svelte'
import { pageLog } from './lib/pagelog'
import './app.css'

// Dev-only: the webview console is unreachable from the shell, and a blank page
// with no error is the worst possible failure mode. Ship anything uncaught to the
// vite middleware, which prints it into the dev log next to the Rust output.
window.addEventListener('error', (e) => {
  pageLog(`UNCAUGHT: ${e.message} @ ${e.filename}:${e.lineno}:${e.colno}`)
})
window.addEventListener('unhandledrejection', (e) => {
  pageLog(`REJECTION: ${String((e as PromiseRejectionEvent).reason)}`)
})

// Which bundle this is, named in the log before anything else decides to do
// something with it. The release binary embeds `ui/dist` as it stands, so the
// only difference between a product build and a harness build is a build-time
// flag baked into bytes - and the boot log has to say which one is running.
pageLog(
  import.meta.env.VITE_SELFTEST
    ? 'boot: main.ts executing (self-test bundle embedded; it stays inert unless the host is armed)'
    : 'boot: main.ts executing (product bundle)',
)

const app = mount(App, {
  target: document.getElementById('app')!,
})

pageLog('boot: mount() returned')

export default app
