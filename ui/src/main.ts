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
  pageLog(`REJECTION: ${String((e as PromiseRejectionHandlingEvent).reason)}`)
})

pageLog('boot: main.ts executing')

const app = mount(App, {
  target: document.getElementById('app')!,
})

pageLog('boot: mount() returned')

export default app
