import { defineConfig } from 'vite'
import tailwindcss from '@tailwindcss/vite'

/**
 * Build for the scroll probe (ui/perf/probe.html).
 *
 * Kept separate from ui/vite.config.ts on purpose: the app bundle must not gain a
 * probe page, and the probe must not gain the app. Run with:
 *   npx vite build --config perf/vite.config.ts --outDir <scratch dir>
 * from ui/ — scripts/perf-rig.mjs does that, serves the result, and drives it in
 * the same WebKitGTK the desktop app links.
 */
export default defineConfig({
  root: new URL('.', import.meta.url).pathname,
  envPrefix: ['VITE_'],
  build: {
    target: 'es2022',
    sourcemap: false,
    minify: 'esbuild',
    rollupOptions: { input: { probe: new URL('./probe.html', import.meta.url).pathname } },
  },
  plugins: [tailwindcss()],
})
