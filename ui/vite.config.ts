import { defineConfig } from 'vite'
import { svelte } from '@sveltejs/vite-plugin-svelte'
import tailwindcss from '@tailwindcss/vite'

// M0 diagnostic: log every module the webview actually requests.
// Without this you cannot tell "the page is blank" from "the page requested
// main.ts and then threw". Set CWOFFEE_REQ_LOG=0 to silence.
const reqLog = {
  name: 'cwoffee-req-log',
  configureServer(server) {
    server.middlewares.use((req, res, next) => {
      const url = req.url || ''

      // Dev-only escape hatch: the webview's console is not reachable from the
      // shell, so the UI POSTs its errors here and they land in the dev log.
      if (url.startsWith('/__cwoffee_log')) {
        let body = ''
        req.on('data', (c) => (body += c))
        req.on('end', () => {
          process.stdout.write(`[cwoffee/page] ${body}\n`)
          res.statusCode = 204
          res.end()
        })
        return
      }

      if (process.env.CWOFFEE_REQ_LOG !== '0' && !url.startsWith('/@vite') && !url.includes('.map')) {
        process.stdout.write(`[cwoffee/vite] -> ${url}\n`)
      }
      next()
    })
  },
}

export default defineConfig({
  // Tauri expects a fixed port; if this is busy the dev server must fail loudly
  // rather than pick 5174 and leave the window pointed at nothing.
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
    watch: { ignored: ['**/target/**', '**/dist/**'] },
  },
  envPrefix: ['VITE_', 'TAURI_'],
  build: {
    target: 'es2022',
    sourcemap: false,
    minify: 'esbuild',
  },
  plugins: [tailwindcss(), svelte({ hot: !process.env.TAURI_ENV_DEBUG }), reqLog],
})
