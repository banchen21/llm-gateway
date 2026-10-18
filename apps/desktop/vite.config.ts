import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

/// Where the gateway listens, for the dev server's proxy. Override when the gateway runs
/// on a different port than its default.
const gateway = process.env.LLM_GATEWAY_URL ?? 'http://127.0.0.1:8787'

export default defineConfig({
  plugins: [vue()],
  // Relative asset URLs so one build works both when the gateway serves it at `/` and
  // when the desktop shell loads it over Tauri's asset protocol.
  base: './',
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
  server: {
    port: 5173,
    proxy: {
      '/admin': gateway,
      '/v1': gateway,
    },
  },
})
